//! `llm-index index` end to end on a temp fixture: real coordinator, real workers, real DBs,
//! searched through a read-only `Store` with `rank::search`.
use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use llm_index::config;
use llm_index::db::{Mode, Store};
use llm_index::rank::{Field, Query, SearchMode, search};

fn query(text: &str) -> Query<'_> {
    Query {
        text,
        mode: SearchMode::Words,
        field: Field::Any,
        folder: None,
        file_type: None,
        after_ns: None,
        before_ns: None,
        limit: 10,
    }
}

/// One MCP tool call against `llm-index mcp --tier public`; returns the result text.
fn mcp_call(cfg: &std::path::Path, tool: &str, args: &str) -> String {
    use std::io::{BufRead, BufReader, Write};
    let mut child = Command::new(env!("CARGO_BIN_EXE_llm-index"))
        .args(["mcp", "--tier", "public", "--config"])
        .arg(cfg)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-03-26","capabilities":{{}},"clientInfo":{{"name":"t","version":"0"}}}}}}"#).unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .unwrap();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"{tool}","arguments":{args}}}}}"#).unwrap();
    stdin.flush().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    let text = loop {
        line.clear();
        assert!(out.read_line(&mut line).unwrap() > 0, "server closed");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        if v["id"] == 2 {
            break v["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string();
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    text
}

fn count(conn: &rusqlite::Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0)
}

#[test]
fn index_makes_searchable_rows_and_keeps_tiers_apart() {
    let base = std::env::temp_dir().join(format!("llm-index-e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let (pubr, privr) = (base.join("pub"), base.join("priv"));
    fs::create_dir_all(&pubr).unwrap();
    fs::create_dir_all(&privr).unwrap();
    fs::write(
        pubr.join("garden.md"),
        "# Garden\n\nTomatoes need watering every morning.\n",
    )
    .unwrap();
    fs::write(
        privr.join("diary.md"),
        "# Diary\n\nThe zeppelin schedule is confidential.\n",
    )
    .unwrap();
    fs::write(pubr.join(".env"), "TOKEN=zeppelin\n").unwrap();
    // unsafe names: a file and a directory (with a safe file inside) are never indexed
    fs::write(pubr.join("rlo\u{202e}file.md"), "quokka in a file\n").unwrap();
    fs::create_dir_all(pubr.join("rlo\u{202e}dir")).unwrap();
    fs::write(pubr.join("rlo\u{202e}dir/inner.md"), "quokka in a dir\n").unwrap();
    fs::write(pubr.join("quokka-sibling.md"), "quokka sibling\n").unwrap();
    // the state dir sits inside a root: it must never be indexed
    let state = pubr.join("idx-state");
    let cfg = base.join("config.toml");
    fs::write(
        &cfg,
        format!(
            "state_dir = \"{}\"\n[[root]]\nname = \"pub\"\npath = \"{}\"\ntier = \"public\"\n\
             [[root]]\nname = \"priv\"\npath = \"{}\"\ntier = \"private\"\n",
            state.display(),
            pubr.display(),
            privr.display()
        ),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_llm-index"))
        .arg("--config")
        .arg(&cfg)
        .arg("index")
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let ready = loop {
        if let Ok(s) = Store::open(&state, Mode::Both) {
            let (p, q) = (&s.public, s.private.as_ref().unwrap());
            if count(p, "SELECT count(*) FROM files") >= 2
                && count(q, "SELECT count(*) FROM files") >= 1
                && count(p, "SELECT count(*) FROM validated") >= 1
            {
                break true;
            }
        }
        if start.elapsed() > Duration::from_secs(90) {
            break false;
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    let _ = child.kill();
    let _ = child.wait();
    assert!(ready, "index produced no rows in time");

    let public = Store::open(&state, Mode::PublicOnly).unwrap();
    public.set_config(config::load(&cfg).unwrap());
    {
        let _snap = public.begin().unwrap();
        public.gate().expect("roots validated");
    }
    let hits = search(&public, &query("tomatoes"), None).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(hits[0].abspath.ends_with("pub/garden.md"));
    assert!(
        search(&public, &query("zeppelin"), None)
            .unwrap()
            .is_empty()
    );
    assert!(search(&public, &query("diary"), None).unwrap().is_empty());
    let hits = search(&public, &query("quokka"), None).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(hits[0].abspath.ends_with("pub/quokka-sibling.md"));
    assert_eq!(
        count(
            &public.public,
            "SELECT count(*) FROM files WHERE relpath LIKE '%' || char(8238) || '%'"
        ),
        0
    );

    let recent = mcp_call(&cfg, "recent", r#"{"days":1}"#);
    assert!(recent.contains("quokka-sibling.md"), "{recent}");
    assert!(
        !recent.contains('\u{202e}') && !recent.contains("rlo"),
        "{recent}"
    );

    let both = Store::open(&state, Mode::Both).unwrap();
    both.set_config(config::load(&cfg).unwrap());
    let hits = search(&both, &query("zeppelin"), None).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(hits[0].abspath.ends_with("priv/diary.md"));
    // no file of the state dir and no denied file reached either database
    for conn in [&both.public, both.private.as_ref().unwrap()] {
        assert_eq!(
            count(
                conn,
                "SELECT count(*) FROM files WHERE abspath LIKE '%idx-state%'"
            ),
            0
        );
        assert_eq!(
            count(
                conn,
                "SELECT count(*) FROM files WHERE abspath LIKE '%.env'"
            ),
            0
        );
    }
    assert_eq!(
        count(
            &both.public,
            "SELECT count(*) FROM roots WHERE name = 'priv'"
        ),
        0
    );
    let _ = fs::remove_dir_all(&base);
}

/// ~10 KB of varied text; `i` and the line number make every line distinct.
fn filler(i: usize) -> String {
    (0..150)
        .map(|l| {
            format!(
                "note {i} line {l}: kestrel{} harbor{} lantern{} orchard{}\n",
                i * 31 + l,
                l * 17 + i,
                i * l,
                (i + l) * 7
            )
        })
        .collect()
}

#[test]
fn removing_a_root_restarts_cleanly_and_compacts() {
    use std::fs::File;
    const N: usize = 400;
    let base = std::env::temp_dir().join(format!("llm-index-e2e-rm-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let (a, b) = (base.join("a"), base.join("b"));
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("alpha.md"), "# Alpha\n\nmarmalade on toast\n").unwrap();
    for i in 0..N {
        fs::write(a.join(format!("n{i}.md")), filler(i)).unwrap();
        fs::write(b.join(format!("n{i}.md")), filler(i + N)).unwrap();
    }
    let state = base.join("state");
    let cfg = base.join("config.toml");
    let write_cfg = |with_b: bool| {
        let mut t = format!(
            "state_dir = \"{}\"\n[[root]]\nname = \"a\"\npath = \"{}\"\ntier = \"public\"\n",
            state.display(),
            a.display()
        );
        if with_b {
            t += &format!(
                "[[root]]\nname = \"b\"\npath = \"{}\"\ntier = \"public\"\n",
                b.display()
            );
        }
        fs::write(&cfg, t).unwrap();
    };
    let run = |log: Option<File>| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_llm-index"));
        c.arg("--config")
            .arg(&cfg)
            .arg("index")
            .env_remove("SQLITE_TMPDIR");
        match log {
            Some(f) => c.stderr(Stdio::from(f)),
            None => c.stderr(Stdio::inherit()),
        };
        c.spawn().unwrap()
    };
    let indexed = "SELECT count(DISTINCT f.id) FROM files f JOIN chunks c ON c.file_id = f.id \
                   WHERE f.root_id = ";
    let marmalade = "SELECT count(*) FROM files f JOIN chunks c ON c.file_id = f.id \
                     JOIN chunks_fts ON chunks_fts.rowid = c.id \
                     WHERE f.relpath = 'alpha.md' AND chunks_fts MATCH 'marmalade'";

    // run 1: both roots
    write_cfg(true);
    let mut child = run(None);
    let start = Instant::now();
    let ready = loop {
        if let Ok(s) = Store::open(&state, Mode::Both) {
            let p = &s.public;
            if count(p, &format!("{indexed}'a'")) == N as i64 + 1
                && count(p, &format!("{indexed}'b'")) == N as i64
                && count(p, marmalade) == 1
            {
                break true;
            }
        }
        if start.elapsed() > Duration::from_secs(300) {
            break false;
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    eprintln!("run 1 took {:?}", start.elapsed());
    let _ = child.kill();
    let _ = child.wait();
    assert!(ready, "run 1 did not index both roots in time");
    let baseline = {
        let s = Store::open(&state, Mode::Both).unwrap();
        count(&s.public, "PRAGMA page_count")
    };

    // run 2: root b removed, no SQLITE_TMPDIR inherited
    write_cfg(false);
    let log = base.join("run2.log");
    let mut child = run(Some(File::create(&log).unwrap()));
    let start = Instant::now();
    let text = loop {
        let t = fs::read_to_string(&log).unwrap_or_default();
        if t.contains("store: compacted public") || t.contains("store: compact public:") {
            break t;
        }
        if start.elapsed() > Duration::from_secs(300) {
            break t;
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    let alive = child.try_wait().unwrap().is_none();
    let s = Store::open(&state, Mode::Both).unwrap();
    let p = &s.public;
    let (pages, free, size) = (
        count(p, "PRAGMA page_count"),
        count(p, "PRAGMA freelist_count"),
        count(p, "PRAGMA page_size"),
    );
    let (b_rows, found) = (
        count(p, "SELECT count(*) FROM files WHERE root_id = 'b'"),
        count(p, marmalade),
    );
    let _ = child.kill();
    let _ = child.wait();
    assert!(text.contains("store: compacted public"), "{text}");
    assert!(alive, "coordinator exited: {text}");
    assert_eq!(b_rows, 0);
    assert_eq!(found, 1);
    assert!(pages < baseline, "{pages} !< {baseline}");
    assert_eq!(free, 0);
    assert!(
        pages * size >= 8 << 20,
        "{} bytes is too small to spill",
        pages * size
    );
    let _ = fs::remove_dir_all(&base);
}
