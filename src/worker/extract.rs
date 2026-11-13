use std::error::Error;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, Instant};

use rustix::fs::{Mode, OFlags, fstat, openat2};

use super::Output;
use super::sandbox::Ready;
use super::scan::{LOCKFILES, RESOLVE, SKIP_DIRS, errno_reason};
use crate::chunk;
use crate::config::Tier;
use crate::deny;
use crate::extract::{self, ExtractError, Extracted, SegmentKind};
use crate::proto::Message;
use crate::sanitize;

const EXTRACT_TIMEOUT: Duration = Duration::from_secs(30);

pub fn extract(
    ready: &Ready,
    relpath: String,
    expected_size: u64,
    expected_mtime_ns: i64,
    output: &Output,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + EXTRACT_TIMEOUT;
    extract_with_deadlines(
        ready,
        relpath,
        expected_size,
        expected_mtime_ns,
        deadline,
        deadline,
        output,
    )
}

fn extract_with_deadlines(
    ready: &Ready,
    relpath: String,
    expected_size: u64,
    expected_mtime_ns: i64,
    extract_deadline: Instant,
    chunk_deadline: Instant,
    output: &Output,
) -> Result<(), Box<dyn Error>> {
    if !normal_relative(&relpath) {
        return output.send(&Message::Failed {
            relpath,
            reason: "invalid_path".into(),
        });
    }

    let root_idx = ready.root.ok_or("missing root")?;
    let root = &ready.config.roots[root_idx];
    let abspath = root.path.join(&relpath);

    // 1. Re-evaluate exclusions, denylist (with lifts only for private roots), and nested-root
    // membership on the absolute path before any filesystem open.
    if let Some(reason) = pre_open_check(ready, root_idx, &abspath, &relpath) {
        return output.send(&Message::Failed {
            relpath,
            reason: reason.into(),
        });
    }

    // 2. Check expected size vs max_file_mb before opening.
    let max_bytes = ready.config.limits.max_file_mb.saturating_mul(1024 * 1024);
    if max_bytes > 0 && expected_size > max_bytes {
        return output.send(&Message::Failed {
            relpath,
            reason: "too_large".into(),
        });
    }

    // 3. Open beneath root_fd without following symlinks, magiclinks, or crossing mounts.
    let root_fd = ready.root_fd.as_ref().ok_or("missing root fd")?;
    let fd = match openat2(
        root_fd,
        relpath.as_str(),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
        RESOLVE,
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::LOOP) => {
            return output.send(&Message::Failed {
                relpath,
                reason: "symlink".into(),
            });
        }
        Err(rustix::io::Errno::XDEV) => {
            return output.send(&Message::Failed {
                relpath,
                reason: "mount".into(),
            });
        }
        Err(err) => {
            return output.send(&Message::Failed {
                relpath,
                reason: errno_reason(err),
            });
        }
    };

    // 4. fstat: must be a regular file and size/mtime_ns must match expected.
    let meta = match fstat(&fd) {
        Ok(meta) => meta,
        Err(err) => {
            return output.send(&Message::Failed {
                relpath,
                reason: errno_reason(err),
            });
        }
    };
    if meta.st_mode & 0o170000 != 0o100000 {
        return output.send(&Message::Failed {
            relpath,
            reason: "special".into(),
        });
    }
    let actual_size = meta.st_size as u64;
    let actual_mtime_ns = (meta.st_mtime as i64)
        .saturating_mul(1_000_000_000)
        .saturating_add(meta.st_mtime_nsec as i64);
    if actual_size != expected_size || actual_mtime_ns != expected_mtime_ns {
        return output.send(&Message::Failed {
            relpath,
            reason: "changed".into(),
        });
    }

    // 5. Read with byte Progress from the I/O loop and 30 s per-job deadline.
    let mut file = File::from(fd);
    let cap = (expected_size as usize).min(16 * 1024 * 1024);
    let mut bytes = Vec::with_capacity(cap);
    let mut buf = [0u8; 64 * 1024];

    loop {
        if Instant::now() >= extract_deadline {
            return output.send(&Message::Failed {
                relpath,
                reason: "timeout".into(),
            });
        }
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                return output.send(&Message::Failed {
                    relpath,
                    reason: err.to_string(),
                });
            }
        };
        bytes.extend_from_slice(&buf[..n]);
        output.add_bytes(n as u64)?;
        if (bytes.len() as u64) > expected_size {
            return output.send(&Message::Failed {
                relpath,
                reason: "changed".into(),
            });
        }
    }

    if (bytes.len() as u64) != expected_size {
        return output.send(&Message::Failed {
            relpath,
            reason: "changed".into(),
        });
    }
    if Instant::now() >= extract_deadline {
        return output.send(&Message::Failed {
            relpath,
            reason: "timeout".into(),
        });
    }

    // 6. detect() then extract() with deadline.
    let file_type = match extract::detect(&relpath, &bytes) {
        Ok(ft) => ft,
        Err(skip) => {
            return output.send(&Message::Failed {
                relpath,
                reason: skip.to_string(),
            });
        }
    };

    let mut extracted = match extract::extract_with_deadline(
        &relpath,
        file_type,
        &bytes,
        &ready.config.limits,
        extract_deadline,
    ) {
        Ok(ext) => ext,
        Err(err) => {
            let reason = match err {
                ExtractError::Skipped(skip) => skip.to_string(),
                ExtractError::ZipBomb => "zip_bomb".into(),
                ExtractError::TooLarge => "too_large".into(),
                ExtractError::Timeout => "timeout".into(),
                ExtractError::Parse(msg) => msg,
            };
            return output.send(&Message::Failed { relpath, reason });
        }
    };

    if Instant::now() >= chunk_deadline {
        return output.send(&Message::Failed {
            relpath,
            reason: "timeout".into(),
        });
    }

    // 7. Bound labels before chunking copies headings across chunks.
    label_extracted(&mut extracted, &relpath);

    // Chunk with deadline, sanitize chunk text (dropping empty chunks),
    // compute BLAKE3 of bytes, re-check deadline, and reply Extracted.
    let mut chunks = match chunk::chunk(&extracted.segments, extracted.file_type, chunk_deadline) {
        Ok(chunks) => chunks,
        Err(ExtractError::Timeout) => {
            return output.send(&Message::Failed {
                relpath,
                reason: "timeout".into(),
            });
        }
        Err(err) => {
            return output.send(&Message::Failed {
                relpath,
                reason: err.to_string(),
            });
        }
    };

    if Instant::now() >= chunk_deadline {
        return output.send(&Message::Failed {
            relpath,
            reason: "timeout".into(),
        });
    }

    sanitize::sanitize_chunks(&mut chunks);

    let blake3 = blake3::hash(&bytes).to_hex().to_string();

    if Instant::now() >= chunk_deadline {
        return output.send(&Message::Failed {
            relpath,
            reason: "timeout".into(),
        });
    }

    output.send(&Message::Extracted {
        relpath,
        title: extracted.title,
        r#type: extracted.file_type.as_str().into(),
        blake3,
        chunks,
    })
}

fn label_extracted(extracted: &mut Extracted, relpath: &str) {
    extracted.title = sanitize::label(&extracted.title, 200);
    if extracted.title.is_empty() {
        extracted.title = sanitize::label(&extract::file_name(relpath), 200);
    }
    for segment in &mut extracted.segments {
        if let SegmentKind::Section { heading } = &mut segment.kind {
            *heading = heading.as_deref().and_then(|s| {
                let cleaned = sanitize::label(s, 200);
                (!cleaned.is_empty()).then_some(cleaned)
            });
        }
    }
}

fn normal_relative(relpath: &str) -> bool {
    !relpath.is_empty()
        && !relpath.starts_with('/')
        && !relpath.contains('\0')
        && relpath
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn pre_open_check(
    ready: &Ready,
    root_idx: usize,
    abspath: &Path,
    relpath: &str,
) -> Option<&'static str> {
    let root = &ready.config.roots[root_idx];

    if ready
        .config
        .exclude
        .paths
        .iter()
        .any(|p| abspath.starts_with(p))
    {
        return Some("excluded");
    }

    let lifts = if root.tier == Tier::Private {
        ready.config.exclude.lift_builtin.as_slice()
    } else {
        &[]
    };
    if deny::denied(&abspath.to_string_lossy(), &ready.config.state_dir, lifts) {
        return Some("denied");
    }

    if ready.config.roots.iter().enumerate().any(|(n, r)| {
        n != root_idx && r.path.starts_with(&root.path) && abspath.starts_with(&r.path)
    }) {
        return Some("nested_root");
    }

    let parts: Vec<&str> = relpath.split('/').collect();
    for &dir_part in &parts[..parts.len().saturating_sub(1)] {
        if SKIP_DIRS.contains(&dir_part) || dir_part.starts_with("result") {
            return Some("directory");
        }
    }
    if let Some(&file_part) = parts.last()
        && LOCKFILES.contains(&file_part)
    {
        return Some("lockfile");
    }

    if ready
        .global_ignore
        .matched_path_or_any_parents(abspath, false)
        .is_ignore()
    {
        return Some("ignored");
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{BufRead, BufReader, Cursor, Write};
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};

    use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, utimensat};
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    use zip::CompressionMethod;
    use zip::write::SimpleFileOptions;

    use crate::proto::Job;
    use crate::worker;

    #[test]
    fn bounds_heading_before_chunking() {
        let mut extracted = Extracted {
            title: "\x1b".into(),
            file_type: extract::FileType::Markdown,
            segments: vec![extract::Segment {
                kind: SegmentKind::Section {
                    heading: Some("H".repeat(100_000)),
                },
                start_line: 1,
                end_line: 1,
                text: "body".into(),
            }],
        };
        label_extracted(&mut extracted, "fallback.md");
        assert_eq!(extracted.title, "fallback.md");
        let SegmentKind::Section { heading } = &extracted.segments[0].kind else {
            panic!("expected section");
        };
        assert_eq!(heading.as_ref().unwrap().chars().count(), 200);
        let chunks = chunk::chunk(
            &extracted.segments,
            extracted.file_type,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(
            chunks[0]
                .heading_or_symbol
                .as_deref()
                .unwrap()
                .chars()
                .count(),
            200
        );
    }

    fn stat_size_mtime(path: &Path) -> (u64, i64) {
        let meta = fs::metadata(path).unwrap();
        let mtime_ns = meta
            .mtime()
            .saturating_mul(1_000_000_000)
            .saturating_add(meta.mtime_nsec());
        (meta.len(), mtime_ns)
    }

    fn build_ods_repeat_bomb() -> Vec<u8> {
        let mimetype = b"application/vnd.oasis.opendocument.spreadsheet";
        let manifest_xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0">
  <manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/>
  <manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/>
</manifest:manifest>"#;
        let content_xml = br#"<?xml version="1.0" encoding="UTF-8"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="Bomb"><table:table-row table:number-rows-repeated="6000"><table:table-cell table:number-columns-repeated="16000" office:value-type="string"><text:p>x</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let mut buf = Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut buf);
            zw.start_file(
                "mimetype",
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
            )
            .unwrap();
            zw.write_all(mimetype).unwrap();

            zw.start_file(
                "META-INF/manifest.xml",
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
            )
            .unwrap();
            zw.write_all(manifest_xml).unwrap();

            zw.start_file(
                "content.xml",
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
            )
            .unwrap();
            zw.write_all(content_xml).unwrap();
            zw.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn extract_checks_child() {
        if std::env::var_os("LLM_INDEX_EXTRACT_CHECKS_CHILD").is_none() {
            return;
        }
        let base = std::env::var("LLM_INDEX_TEST_BASE").unwrap();
        let base_path = Path::new(&base);

        let (orig_size, orig_mtime) = stat_size_mtime(&base_path.join("root/changed.txt"));
        let (same_size, same_mtime) = stat_size_mtime(&base_path.join("root/same_size_mtime.txt"));
        let (swap_size, swap_mtime) = stat_size_mtime(&base_path.join("root/sub/target.txt"));
        let (ok_size, ok_mtime) = stat_size_mtime(&base_path.join("root/hello.md"));
        let (slow_size, slow_mtime) = stat_size_mtime(&base_path.join("root/slow.rs"));

        // Acquire root_fd and apply Landlock before swapping `root/sub` and modifying files.
        let ready =
            worker::sandbox::start(&base_path.join("config.toml"), "extract", Some("parent"))
                .unwrap();

        let output = Output::new();

        // Readiness handshake: emit Entry so parent knows root_fd and Landlock are active,
        // then wait for parent to finish swapping `root/sub` and updating test files.
        output
            .send(&Message::Entry {
                relpath: "sub/target.txt".into(),
                size: swap_size,
                mtime_ns: swap_mtime,
                is_dir: false,
            })
            .unwrap();
        let mut go = String::new();
        io::stdin().read_line(&mut go).unwrap();

        // 1. Excluded file (FIFO with no writer): must NEVER be opened, returning quickly with "excluded".
        extract(&ready, "excluded/fifo.txt".into(), 10, 12345, &output).unwrap();

        // 2. Denied file (FIFO with no writer): must NEVER be opened, returning quickly with "denied".
        extract(&ready, ".env".into(), 10, 12345, &output).unwrap();

        // 3. Nested root file (FIFO with no writer): must NEVER be opened, returning quickly with "nested_root".
        extract(&ready, "child/fifo.txt".into(), 10, 12345, &output).unwrap();

        // 4. Directory swapped for a symlink between Entry and Extract: must not be read.
        extract(
            &ready,
            "sub/target.txt".into(),
            swap_size,
            swap_mtime,
            &output,
        )
        .unwrap();

        // 5. File whose size changed since scan: Failed "changed".
        extract(&ready, "changed.txt".into(), orig_size, orig_mtime, &output).unwrap();

        // 6. File whose size is identical but mtime_ns changed since scan: Failed "changed".
        extract(
            &ready,
            "same_size_mtime.txt".into(),
            same_size,
            same_mtime,
            &output,
        )
        .unwrap();

        // 7. Deadline firing during chunking (tiny chunk deadline): Failed "timeout".
        let now = Instant::now();
        extract_with_deadlines(
            &ready,
            "slow.rs".into(),
            slow_size,
            slow_mtime,
            now + Duration::from_secs(30),
            now,
            &output,
        )
        .unwrap();

        // 8. Normal file with invisible characters in title, an invisible-only section (dropped),
        // and a body section: Extracted and sanitized.
        extract(&ready, "hello.md".into(), ok_size, ok_mtime, &output).unwrap();
    }

    #[test]
    fn excluded_never_opened_swapped_symlink_blocked_and_changed_detected() {
        let base =
            std::env::temp_dir().join(format!("llm-index-extract-checks-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("root/excluded")).unwrap();
        fs::create_dir_all(base.join("root/child")).unwrap();
        fs::create_dir_all(base.join("root/sub")).unwrap();
        fs::create_dir_all(base.join("outside")).unwrap();

        // Create FIFOs with no writer at excluded, denied, and nested-root paths.
        // Opening any of them without O_NONBLOCK would hang; even with O_NONBLOCK, pre-open
        // checks must reject them with "excluded" / "denied" / "nested_root" (not "special").
        for rel in ["root/excluded/fifo.txt", "root/.env", "root/child/fifo.txt"] {
            rustix::fs::mkfifoat(
                rustix::fs::CWD,
                base.join(rel),
                rustix::fs::Mode::from_bits_retain(0o600),
            )
            .unwrap();
        }

        fs::write(base.join("root/sub/target.txt"), "inside content").unwrap();
        fs::write(base.join("outside/target.txt"), "outside secret").unwrap();
        fs::write(base.join("root/changed.txt"), "before change").unwrap();
        fs::write(base.join("root/same_size_mtime.txt"), "1234567890123").unwrap();
        fs::write(base.join("root/slow.rs"), "pub fn slow() -> i32 { 42 }\n").unwrap();
        fs::write(
            base.join("root/hello.md"),
            concat!(
                "\u{200E}\u{200F}\u{061C}\u{2061}\u{2062}\u{2063}\u{2064}\u{180E}\u{FE00}\u{FE0F}\u{E0100}\u{E01EF}\n\n",
                "# Clean\u{200B}Title\u{202E}\u{2061}\u{FE0F}\n\n",
                "Paragraph with \u{FEFF}hidden\u{E0041}\u{2063}\u{FE0E}\u{E0101} tags.\n"
            ),
        )
        .unwrap();

        let config_path = base.join("config.toml");
        fs::write(
            &config_path,
            format!(
                "state_dir = \"/tmp/state\"\n[[root]]\nname = \"parent\"\npath = \"{}\"\n[[root]]\nname = \"child\"\npath = \"{}\"\n[exclude]\npaths = [\"{}\"]\n",
                base.join("root").display(),
                base.join("root/child").display(),
                base.join("root/excluded").display(),
            ),
        )
        .unwrap();

        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "worker::extract::tests::extract_checks_child",
                "--nocapture",
            ])
            .env("LLM_INDEX_EXTRACT_CHECKS_CHILD", "1")
            .env("LLM_INDEX_TEST_BASE", &base)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let mut child_stdout = BufReader::new(child.stdout.take().unwrap());
        let mut ready_seen = false;
        let mut line = String::new();
        while child_stdout.read_line(&mut line).unwrap() > 0 {
            if line.contains("\"message\":\"Entry\"") && line.contains("sub/target.txt") {
                ready_seen = true;
                break;
            }
            line.clear();
        }
        assert!(ready_seen, "did not receive readiness Entry from child");

        // Swap `root/sub` for a symlink, modify `changed.txt`, and update `same_size_mtime.txt`
        // with the exact same byte length and a new mtime.
        fs::rename(base.join("root/sub"), base.join("moved_sub")).unwrap();
        std::os::unix::fs::symlink(base.join("outside"), base.join("root/sub")).unwrap();
        fs::write(
            base.join("root/changed.txt"),
            "after change with more bytes",
        )
        .unwrap();
        let same_path = base.join("root/same_size_mtime.txt");
        let (_, old_mtime_ns) = stat_size_mtime(&same_path);
        fs::write(&same_path, "abcdefghijklm").unwrap();
        let new_sec = (old_mtime_ns / 1_000_000_000) + 10;
        let ts = Timestamps {
            last_access: Timespec {
                tv_sec: new_sec,
                tv_nsec: 0,
            },
            last_modification: Timespec {
                tv_sec: new_sec,
                tv_nsec: 123_456_789,
            },
        };
        utimensat(CWD, &same_path, &ts, AtFlags::empty()).unwrap();
        let (new_size, new_mtime_ns) = stat_size_mtime(&same_path);
        assert_eq!(new_size, 13);
        assert_ne!(new_mtime_ns, old_mtime_ns);

        // Signal child to proceed.
        {
            let mut stdin = child.stdin.take().unwrap();
            writeln!(stdin, "go").unwrap();
        }

        let mut rest_stdout = String::new();
        child_stdout.read_to_string(&mut rest_stdout).unwrap();
        let status = child.wait().unwrap();
        assert!(status.success(), "child failed with status {status}");
        let stdout = rest_stdout;
        assert!(stdout.contains("1 passed"), "{stdout}");
        assert!(
            stdout.contains(
                "\"message\":\"Failed\",\"relpath\":\"excluded/fifo.txt\",\"reason\":\"excluded\""
            ),
            "{stdout}"
        );
        assert!(
            stdout.contains("\"message\":\"Failed\",\"relpath\":\".env\",\"reason\":\"denied\""),
            "{stdout}"
        );
        assert!(
            stdout.contains(
                "\"message\":\"Failed\",\"relpath\":\"child/fifo.txt\",\"reason\":\"nested_root\""
            ),
            "{stdout}"
        );
        assert!(
            stdout.contains(
                "\"message\":\"Failed\",\"relpath\":\"sub/target.txt\",\"reason\":\"symlink\""
            ),
            "{stdout}"
        );
        assert!(!stdout.contains("outside secret"), "{stdout}");
        assert!(
            stdout.contains(
                "\"message\":\"Failed\",\"relpath\":\"changed.txt\",\"reason\":\"changed\""
            ),
            "{stdout}"
        );
        assert!(
            stdout.contains(
                "\"message\":\"Failed\",\"relpath\":\"same_size_mtime.txt\",\"reason\":\"changed\""
            ),
            "{stdout}"
        );
        assert!(
            stdout
                .contains("\"message\":\"Failed\",\"relpath\":\"slow.rs\",\"reason\":\"timeout\""),
            "{stdout}"
        );
        assert!(
            stdout.contains(
                "\"message\":\"Extracted\",\"relpath\":\"hello.md\",\"title\":\"CleanTitle\""
            ),
            "{stdout}"
        );
        assert!(stdout.contains("Paragraph with hidden tags."), "{stdout}");
        // The first section of hello.md contained only stripped invisible characters, so it must
        // be dropped, leaving only the 1 real section chunk.
        let extracted_line = stdout
            .lines()
            .find(|l| l.contains("\"message\":\"Extracted\"") && l.contains("hello.md"))
            .expect("Extracted message for hello.md");
        let parsed: serde_json::Value = serde_json::from_str(extracted_line).unwrap();
        let chunks = parsed["chunks"].as_array().unwrap();
        assert_eq!(
            chunks.len(),
            1,
            "expected empty sanitized chunk to be dropped: {chunks:?}"
        );
        assert_eq!(chunks[0]["ord"], 0);
        assert_eq!(chunks[0]["heading_or_symbol"], "CleanTitle");

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn extract_worker_child() {
        if std::env::var_os("LLM_INDEX_EXTRACT_WORKER_CHILD").is_none() {
            return;
        }
        let base = std::env::var("LLM_INDEX_TEST_BASE").unwrap();
        // Set 1 GiB RLIMIT_AS before running the extract worker.
        let old = getrlimit(Resource::As);
        let one_gib: u64 = 1024 * 1024 * 1024;
        setrlimit(
            Resource::As,
            Rlimit {
                current: Some(one_gib.min(old.maximum.unwrap_or(one_gib))),
                maximum: Some(one_gib.min(old.maximum.unwrap_or(one_gib))),
            },
        )
        .unwrap();
        worker::run(
            &Path::new(&base).join("config.toml"),
            "extract",
            Some("root"),
        )
        .unwrap();
    }

    #[test]
    fn ods_repeat_bomb_contained_by_worker_process_boundary() {
        let base = std::env::temp_dir().join(format!("llm-index-ods-bomb-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("root")).unwrap();

        let bomb_bytes = build_ods_repeat_bomb();
        assert!(
            bomb_bytes.len() < 1024,
            "expected compact ODS repeat bomb (~726 bytes), got {} bytes",
            bomb_bytes.len()
        );
        let bomb_path = base.join("root/bomb.ods");
        fs::write(&bomb_path, &bomb_bytes).unwrap();
        let (bomb_size, bomb_mtime) = stat_size_mtime(&bomb_path);

        let normal_path = base.join("root/normal.txt");
        fs::write(&normal_path, "Normal document after bomb.\n").unwrap();
        let (normal_size, normal_mtime) = stat_size_mtime(&normal_path);

        fs::write(
            base.join("config.toml"),
            format!(
                "state_dir = \"/tmp/state\"\n[[root]]\nname = \"root\"\npath = \"{}\"\n",
                base.join("root").display()
            ),
        )
        .unwrap();

        // 1. Feed the ODS repeat bomb to a real extract worker child process under 1 GiB RLIMIT_AS.
        let mut bomb_worker = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "worker::extract::tests::extract_worker_child",
                "--nocapture",
            ])
            .env("LLM_INDEX_EXTRACT_WORKER_CHILD", "1")
            .env("LLM_INDEX_TEST_BASE", &base)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let bomb_job = serde_json::to_string(&Job::Extract {
            relpath: "bomb.ods".into(),
            expected_size: bomb_size,
            expected_mtime_ns: bomb_mtime,
        })
        .unwrap();
        {
            let mut stdin = bomb_worker.stdin.take().unwrap();
            let _ = writeln!(stdin, "{bomb_job}");
            let _ = writeln!(stdin, "{}", serde_json::to_string(&Job::Shutdown).unwrap());
        }
        let bomb_out = bomb_worker.wait_with_output().unwrap();
        let bomb_stdout = String::from_utf8_lossy(&bomb_out.stdout);
        let bomb_stderr = String::from_utf8_lossy(&bomb_out.stderr);
        let died_by_signal = bomb_out.status.signal().is_some();
        let alloc_failed_stderr = bomb_stderr.contains("memory allocation of")
            || bomb_stderr.contains("allocation failed")
            || bomb_stderr.contains("out of memory");
        assert!(
            died_by_signal || alloc_failed_stderr,
            "expected bomb worker to die by a signal or report allocation failure on stderr, status={}, signal={:?}, stderr={bomb_stderr}",
            bomb_out.status,
            bomb_out.status.signal()
        );
        assert!(
            !bomb_stdout.contains("\"message\":\"Extracted\""),
            "bomb worker must not send Extracted: {bomb_stdout}"
        );

        // 2. Spawn a fresh extract worker and verify it cleanly extracts the next normal file.
        let mut fresh_worker = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "worker::extract::tests::extract_worker_child",
                "--nocapture",
            ])
            .env("LLM_INDEX_EXTRACT_WORKER_CHILD", "1")
            .env("LLM_INDEX_TEST_BASE", &base)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let normal_job = serde_json::to_string(&Job::Extract {
            relpath: "normal.txt".into(),
            expected_size: normal_size,
            expected_mtime_ns: normal_mtime,
        })
        .unwrap();
        {
            let mut stdin = fresh_worker.stdin.take().unwrap();
            writeln!(stdin, "{normal_job}").unwrap();
            writeln!(stdin, "{}", serde_json::to_string(&Job::Shutdown).unwrap()).unwrap();
        }
        let fresh_out = fresh_worker.wait_with_output().unwrap();
        assert!(
            fresh_out.status.success(),
            "{}",
            String::from_utf8_lossy(&fresh_out.stderr)
        );
        let fresh_stdout = String::from_utf8_lossy(&fresh_out.stdout);
        assert!(fresh_stdout.contains("1 passed"), "{fresh_stdout}");
        assert!(
            fresh_stdout.contains("\"message\":\"Extracted\",\"relpath\":\"normal.txt\""),
            "{fresh_stdout}"
        );
        assert!(
            fresh_stdout.contains("Normal document after bomb."),
            "{fresh_stdout}"
        );

        fs::remove_dir_all(&base).unwrap();
    }
}
