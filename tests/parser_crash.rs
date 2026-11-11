//! Real workers through the real `ProcSpawner`: a spreadsheet bomb must end `Failed` without a
//! retry loop and must not stop the next file from being extracted.
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Cursor, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use llm_index::config::Kind;
use llm_index::index::sched::{Params, RootSpec, Sched};
use llm_index::index::{ExtractedFile, ProcSpawner, ScanResult, Sink};
use llm_index::proto::Share;
use zip::{CompressionMethod, write::SimpleFileOptions};

#[derive(Default)]
struct Mem {
    extracted: Vec<String>,
    failed: Vec<String>,
    scans: usize,
    expected: HashMap<String, Share>,
}

impl Sink for Mem {
    fn indexed_files(&self, _: &str) -> usize {
        0
    }
    fn expected_identity(&self, root: &str) -> Option<Share> {
        self.expected.get(root).cloned()
    }
    fn set_expected_identity(&mut self, root: &str, share: &Share) {
        self.expected.insert(root.into(), share.clone());
    }
    fn known(&self, _: &str, _: &str) -> Option<(u64, i64)> {
        None
    }
    fn scan_done(&mut self, _: &str, _: ScanResult) {
        self.scans += 1;
    }
    fn extracted(&mut self, _: &str, f: ExtractedFile) {
        self.extracted.push(f.relpath);
    }
    fn failed(&mut self, _: &str, relpath: &str, _: u64, _: i64, _: &str) {
        self.failed.push(relpath.into());
    }
    fn validated(&mut self, _: &str, _: &str, _: llm_index::proto::ValidationResult) {}
    fn root_state(&mut self, _: &str, _: &str, _: &str) {}
}

fn ods_bomb() -> Vec<u8> {
    let manifest = br#"<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0">
  <manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/>
  <manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/>
</manifest:manifest>"#;
    let content = br#"<?xml version="1.0" encoding="UTF-8"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Bomb"><table:table-row table:number-rows-repeated="6000"><table:table-cell table:number-columns-repeated="16000" office:value-type="string"><text:p>x</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;
    let mut buf = Cursor::new(Vec::new());
    let mut zw = zip::ZipWriter::new(&mut buf);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    zw.start_file("mimetype", stored).unwrap();
    zw.write_all(b"application/vnd.oasis.opendocument.spreadsheet")
        .unwrap();
    zw.start_file("META-INF/manifest.xml", deflated).unwrap();
    zw.write_all(manifest).unwrap();
    zw.start_file("content.xml", deflated).unwrap();
    zw.write_all(content).unwrap();
    zw.finish().unwrap();
    buf.into_inner()
}

#[test]
fn bomb_fails_once_and_the_next_file_is_extracted() {
    let base: PathBuf =
        std::env::temp_dir().join(format!("llm-index-crash-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let root = base.join("root");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("bomb.ods"), ods_bomb()).unwrap();
    fs::write(root.join("normal.txt"), "Normal document after bomb.\n").unwrap();
    let config = base.join("config.toml");
    fs::write(
        &config,
        format!(
            "state_dir = \"{}\"\n[[root]]\nname = \"r\"\npath = \"{}\"\n",
            base.join("state").display(),
            root.display()
        ),
    )
    .unwrap();
    // Workers run under 1 GiB of address space, as the extract unit test does.
    let wrapper = base.join("worker.sh");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nulimit -v 1048576\nexec {} \"$@\"\n",
            env!("CARGO_BIN_EXE_llm-index")
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();

    let mut mountinfo = File::open("/proc/self/mountinfo").unwrap();
    let mut sched = Sched::new(
        Params {
            workers: 2,
            backoff_max: Duration::from_secs(3600),
        },
        vec![RootSpec {
            name: "r".into(),
            path: root.clone(),
            kind: Kind::Local,
            interval: Duration::from_secs(900),
            share: None,
        }],
        &mut mountinfo,
        ProcSpawner::with_exe(wrapper, &config).unwrap(),
        Mem::default(),
        Box::new(|_, _| true),
    );
    let start = Instant::now();
    loop {
        let (changed, events) = sched
            .spawner
            .poll(&mountinfo, Duration::from_millis(100))
            .unwrap();
        let now = start.elapsed();
        if changed {
            sched.rederive(&mut mountinfo, now);
        }
        for (id, ev) in events {
            sched.handle(id, ev, now);
        }
        sched.tick(now);
        let m = &sched.sink;
        if m.extracted.contains(&"normal.txt".to_string()) && m.failed.contains(&"bomb.ods".into())
        {
            break;
        }
        assert!(now < Duration::from_secs(120), "timed out: {:?}", m.failed);
    }
    // no retry loop: keep running a while longer and the bomb is still failed exactly once
    let until = Instant::now() + Duration::from_secs(4);
    while Instant::now() < until {
        let (_, events) = sched
            .spawner
            .poll(&mountinfo, Duration::from_millis(100))
            .unwrap();
        let now = start.elapsed();
        for (id, ev) in events {
            sched.handle(id, ev, now);
        }
        sched.tick(now);
    }
    assert_eq!(sched.sink.failed, ["bomb.ods"]);
    assert_eq!(sched.sink.extracted, ["normal.txt"]);
    drop(sched);
    let _ = fs::remove_dir_all(&base);
}
