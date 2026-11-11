//! Scripted stdio sessions against a fixture index built with a `Writer` store.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use llm_index::auth::{PUBLIC_CHECKING, path_hash};
use llm_index::db::{Mode, Store};
use llm_index::embed::{DIM, Embedder, Mode as EmbedMode};
use serde_json::{Value, json};

struct Fixture {
    cfg: PathBuf,
    writer: Store,
    embedder: Option<Embedder>,
}

const CONFIG: &str = "[[root]]\nname = \"docs\"\npath = \"/home/x/docs\"\ntier = \"public\"\n\
                      [[root]]\nname = \"vault\"\npath = \"/home/x/vault\"\ntier = \"private\"\n";

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("llm-index-mcp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let state = dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let cfg = dir.join("config.toml");
        std::fs::write(
            &cfg,
            format!("state_dir = {:?}\n{CONFIG}", state.display().to_string()),
        )
        .unwrap();
        let writer = Store::open(&state, Mode::Writer).unwrap();
        for p in ["/home/x/docs", "/home/x/vault"] {
            let h = path_hash(Path::new(p)).unwrap();
            writer
                .public
                .execute(
                    "INSERT INTO validated VALUES(?, 0)",
                    [h.as_bytes().as_slice()],
                )
                .unwrap();
        }
        let m = llm_index::model_dir();
        let embedder =
            (m != "unset").then(|| Embedder::open(Path::new(m), EmbedMode::Query).unwrap());
        let fx = Fixture {
            cfg,
            writer,
            embedder,
        };
        let (p, v) = (&fx.writer.public, fx.writer.private.as_ref().unwrap());
        p.execute("INSERT INTO roots(name, path, state, last_scan, queued, done, skipped_symlinks) VALUES('docs', '/home/x/docs', 'ok', 5, 0, 5, 2)", []).unwrap();
        v.execute("INSERT INTO roots(name, path, state, last_scan, queued, done, skipped_symlinks) VALUES('vault', '/home/x/vault', 'ok', 6, 1, 1, 0)", []).unwrap();
        v.execute(
            "INSERT INTO config_errors VALUES('/home/x/vault/loop', 'symlink')",
            [],
        )
        .unwrap();
        fx.add(
            false,
            "docs",
            "/home/x/docs/cats.md",
            "Cats",
            "markdown",
            "section",
            1,
            3,
            Some("Kittens"),
            "Kittens are small domestic cats.\nThey purr.\nThey nap.",
        );
        fx.add(
            false,
            "docs",
            "/home/x/docs/db.md",
            "Databases",
            "markdown",
            "section",
            1,
            2,
            None,
            "Indexing a database table speeds up queries.\nUse B-trees.",
        );
        fx.add(
            false,
            "docs",
            "/home/x/docs/report.pdf",
            "Annual report",
            "pdf",
            "page",
            1,
            1,
            None,
            "Revenue grew in the first quarter.",
        );
        fx.add(
            false,
            "docs",
            "/home/x/docs/report.pdf",
            "Annual report",
            "pdf",
            "page",
            2,
            2,
            None,
            "Costs fell in the second quarter.",
        );
        fx.add(
            false,
            "docs",
            "/home/x/docs/sheet.xlsx",
            "sheet.xlsx",
            "xlsx",
            "sheet",
            1,
            3,
            Some("Q1"),
            "region,total\nnorth,10\nsouth,20",
        );
        let long: String = (1..=800)
            .map(|i| format!("line {i:04} {}\n", "x".repeat(40)))
            .collect();
        fx.add(
            false,
            "docs",
            "/home/x/docs/long.txt",
            "long.txt",
            "text",
            "section",
            1,
            800,
            None,
            long.trim_end(),
        );
        fx.add(
            true,
            "vault",
            "/home/x/vault/plans.md",
            "Vault plans",
            "markdown",
            "section",
            1,
            1,
            None,
            "Kittens roadmap hidden in the vault.",
        );
        fx
    }

    fn vec_of(&self, text: &str) -> Vec<u8> {
        match &self.embedder {
            Some(e) => e
                .embed(text)
                .unwrap()
                .map_or(vec![0; DIM], |v| v.iter().map(|&x| x as u8).collect()),
            None => vec![0; DIM],
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add(
        &self,
        private: bool,
        root: &str,
        path: &str,
        title: &str,
        ty: &str,
        kind: &str,
        start: i64,
        end: i64,
        sym: Option<&str>,
        text: &str,
    ) {
        let c = if private {
            self.writer.private.as_ref().unwrap()
        } else {
            &self.writer.public
        };
        let fid: i64 = match c.query_row("SELECT id FROM files WHERE abspath = ?1", [path], |r| {
            r.get(0)
        }) {
            Ok(id) => id,
            Err(_) => {
                c.execute("INSERT INTO files(root_id, relpath, abspath, title, type, mtime_ns) VALUES(?1, ?2, ?2, ?3, ?4, ?5)", rusqlite::params![root, path, title, ty, now_ns()]).unwrap();
                let id = c.last_insert_rowid();
                c.execute(
                    "INSERT INTO files_fts(rowid, title, path) VALUES(?1, ?2, ?3)",
                    rusqlite::params![id, title, path],
                )
                .unwrap();
                id
            }
        };
        let ord: i64 = c
            .query_row(
                "SELECT count(*) FROM chunks WHERE file_id = ?1",
                [fid],
                |r| r.get(0),
            )
            .unwrap();
        c.execute("INSERT INTO chunks(file_id, ord, kind, start, end, heading_or_symbol, text, vec) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)", rusqlite::params![fid, ord, kind, start, end, sym, text, self.vec_of(text)]).unwrap();
        let cid = c.last_insert_rowid();
        c.execute(
            "INSERT INTO chunks_fts(rowid, text) VALUES(?1, ?2)",
            rusqlite::params![cid, text],
        )
        .unwrap();
    }

    fn session(&self, tier: &str) -> Session {
        let mut child = Command::new(env!("CARGO_BIN_EXE_llm-index"))
            .args(["mcp", "--tier", tier, "--config"])
            .arg(&self.cfg)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut s = Session {
            stdin: child.stdin.take().unwrap(),
            out: BufReader::new(child.stdout.take().unwrap()),
            child,
            id: 0,
            seen: String::new(),
        };
        s.request("initialize", json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}));
        s.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        s
    }
}

struct Session {
    child: Child,
    stdin: ChildStdin,
    out: BufReader<ChildStdout>,
    id: u64,
    /// Everything the server said, to check for leaks.
    seen: String,
}

impl Session {
    fn send(&mut self, v: &Value) {
        writeln!(self.stdin, "{v}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let id = self.id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let mut line = String::new();
            assert!(self.out.read_line(&mut line).unwrap() > 0, "server closed");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == id {
                self.seen.push_str(&line);
                return v["result"].clone();
            }
        }
    }

    /// (is_error, text)
    fn call(&mut self, tool: &str, args: Value) -> (bool, String) {
        let r = self.request("tools/call", json!({"name": tool, "arguments": args}));
        (
            r["isError"] == true,
            r["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        )
    }

    fn ok(&mut self, tool: &str, args: Value) -> Value {
        let (err, text) = self.call(tool, args);
        assert!(!err, "{tool} failed: {text}");
        serde_json::from_str(&text).unwrap()
    }

    fn rss_mb(&self) -> f64 {
        let s = std::fs::read_to_string(format!("/proc/{}/status", self.child.id())).unwrap();
        let kb: f64 = s
            .lines()
            .find(|l| l.starts_with("VmRSS:"))
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        kb / 1024.0
    }
}

fn paths(v: &Value) -> Vec<String> {
    v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["path"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn public_session_reveals_nothing_private() {
    let fx = Fixture::new("public");
    let mut s = fx.session("public");

    let tools = s.request("tools/list", json!({}));
    let names: Vec<_> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    for n in ["search", "get", "recent", "status"] {
        assert!(names.contains(&n.to_string()), "{names:?}");
    }
    assert!(
        tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["annotations"]["readOnlyHint"] == true)
    );
    let idle = s.rss_mb();

    // words, with the passage wrapped as untrusted content
    let r = s.ok("search", json!({"query": "kittens", "mode": "words"}));
    assert_eq!(paths(&r), ["/home/x/docs/cats.md"]);
    let hit = &r["hits"][0];
    assert!(
        hit["passage"]
            .as_str()
            .unwrap()
            .starts_with("<untrusted-document path=\"/home/x/docs/cats.md\">")
    );
    assert_eq!(hit["lines"], json!([1, 3]));
    assert_eq!(hit["heading"], "Kittens");
    assert!(hit.get("tier").is_none());
    // fields and filters
    let r = s.ok("search", json!({"query": "annual", "field": "title"}));
    assert_eq!(paths(&r), ["/home/x/docs/report.pdf"]);
    let r = s.ok(
        "search",
        json!({"query": "sheet", "field": "path", "mode": "meaning"}),
    );
    assert_eq!(paths(&r), ["/home/x/docs/sheet.xlsx"]);
    assert_eq!(r["hits"][0]["rows"], json!([1, 3]));
    assert_eq!(r["hits"][0]["sheet"], "Q1");
    let r = s.ok(
        "search",
        json!({"query": "quarter", "type": "pdf", "folder": "/home/x/docs", "limit": 1}),
    );
    assert_eq!(r["hits"].as_array().unwrap().len(), 1);
    assert!(r["hits"][0]["page"].as_i64().is_some());
    assert!(
        s.ok(
            "search",
            json!({"query": "kittens", "folder": "/home/x/do"})
        )["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        s.call("search", json!({"query": "x", "folder": "relative"}))
            .0
    );
    // meaning and hybrid
    if fx.embedder.is_some() {
        let r = s.ok("search", json!({"query": "feline pets", "mode": "meaning"}));
        assert_eq!(paths(&r)[0], "/home/x/docs/cats.md");
        let r = s.ok(
            "search",
            json!({"query": "database index", "mode": "hybrid"}),
        );
        assert_eq!(paths(&r)[0], "/home/x/docs/db.md");
    }

    // get with next
    let g = s.ok("get", json!({"path": "/home/x/docs/long.txt"}));
    let first = g["text"].as_str().unwrap();
    assert!(first.chars().count() < 20_200);
    let next = g["next"]["start"].as_u64().unwrap();
    assert!(next > 100 && next < 800);
    let g2 = s.ok(
        "get",
        json!({"path": "/home/x/docs/long.txt", "start": next, "end": next + 1}),
    );
    assert!(
        g2["text"]
            .as_str()
            .unwrap()
            .contains(&format!("line {next:04}"))
    );
    assert!(g2["next"].is_null());
    let g = s.ok(
        "get",
        json!({"path": "/home/x/docs/report.pdf", "start": 2}),
    );
    assert!(
        g["text"].as_str().unwrap().contains("second quarter")
            && !g["text"].as_str().unwrap().contains("first quarter")
    );
    let g = s.ok(
        "get",
        json!({"path": "/home/x/docs/sheet.xlsx", "sheet": "Q1"}),
    );
    assert!(g["text"].as_str().unwrap().contains("north,10"));
    // hidden and missing files look the same
    let hidden = s.call("get", json!({"path": "/home/x/vault/plans.md"}));
    assert_eq!(
        hidden,
        s.call("get", json!({"path": "/home/x/docs/nothing.md"}))
    );
    assert!(hidden.0);

    // recent and status
    let r = s.ok("recent", json!({"days": 1}));
    assert_eq!(r["files"].as_array().unwrap().len(), 5);
    assert_eq!(
        s.ok("recent", json!({"days": 1, "type": "pdf"}))["files"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let st = s.ok("status", json!({}));
    assert_eq!(st["roots"].as_array().unwrap().len(), 1);
    assert_eq!(st["roots"][0]["name"], "docs");
    assert_eq!(st["roots"][0]["files"], 5);
    assert_eq!(st["roots"][0]["skipped_symlinks"], 2);
    assert_eq!(st["databases"].as_array().unwrap().len(), 1);
    assert!(st.get("config_errors").is_none());

    // a row written after the session opened shows up in the next search
    assert!(
        s.ok("search", json!({"query": "zebra", "mode": "words"}))["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    fx.add(
        false,
        "docs",
        "/home/x/docs/zebra.md",
        "Zebra",
        "markdown",
        "section",
        1,
        1,
        None,
        "A zebra crossing.",
    );
    assert_eq!(
        paths(&s.ok("search", json!({"query": "zebra", "mode": "words"}))),
        ["/home/x/docs/zebra.md"]
    );

    let warm = s.rss_mb();
    println!("mcp RSS idle {idle:.1} MB, warm {warm:.1} MB");
    assert!(warm < 100.0, "warm RSS {warm} MB");
    for secret in ["vault", "Vault", "roadmap", "loop"] {
        assert!(!s.seen.contains(secret), "leaked {secret}");
    }
}

#[test]
fn private_session_sees_both_tiers() {
    let fx = Fixture::new("private");
    let mut s = fx.session("private");
    let r = s.ok(
        "search",
        json!({"query": "kittens", "mode": "words", "limit": 5}),
    );
    assert_eq!(r["hits"].as_array().unwrap().len(), 2);
    let tiers: Vec<_> = r["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["tier"].as_str().unwrap())
        .collect();
    assert!(tiers.contains(&"public") && tiers.contains(&"private"));
    assert!(
        s.ok("get", json!({"path": "/home/x/vault/plans.md"}))["text"]
            .as_str()
            .unwrap()
            .contains("roadmap")
    );
    assert_eq!(
        s.ok("recent", json!({"days": 1}))["files"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
    let st = s.ok("status", json!({}));
    assert_eq!(st["roots"].as_array().unwrap().len(), 2);
    assert_eq!(st["config_errors"][0]["path"], "/home/x/vault/loop");
    assert_eq!(st["databases"].as_array().unwrap().len(), 2);
}

#[test]
fn corrupt_config_closes_every_public_tool_with_the_generic_error() {
    let fx = Fixture::new("corrupt");
    let mut s = fx.session("public");
    assert!(!s.call("status", json!({})).0);
    let good = std::fs::read_to_string(&fx.cfg).unwrap();
    std::fs::write(&fx.cfg, "state_dir = [ not toml /home/x/vault").unwrap();
    for (tool, args) in [
        ("search", json!({"query": "kittens"})),
        ("get", json!({"path": "/home/x/docs/cats.md"})),
        ("recent", json!({"days": 1})),
        ("status", json!({})),
    ] {
        assert_eq!(
            s.call(tool, args),
            (true, PUBLIC_CHECKING.to_string()),
            "{tool}"
        );
    }
    // a root added to the config but not yet validated closes the public tools too
    std::fs::write(
        &fx.cfg,
        format!("{good}[[root]]\nname = \"new\"\npath = \"/home/x/new\"\ntier = \"public\"\n"),
    )
    .unwrap();
    assert_eq!(
        s.call("search", json!({"query": "kittens"})),
        (true, PUBLIC_CHECKING.to_string())
    );
    std::fs::write(&fx.cfg, format!("{good}\n")).unwrap();
    assert!(!s.call("search", json!({"query": "kittens"})).0);
    assert!(!s.seen.contains("toml"));
}

#[test]
fn status_cli_and_no_private_db_option() {
    let fx = Fixture::new("cli");
    let out = Command::new(env!("CARGO_BIN_EXE_llm-index"))
        .args(["status", "--config"])
        .arg(&fx.cfg)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains("vault") && text.contains("/home/x/vault/loop") && text.contains("docs"),
        "{text}"
    );
    assert!(text.contains("sandbox:") && text.contains("private.db"));
    let bad = Command::new(env!("CARGO_BIN_EXE_llm-index"))
        .args(["mcp", "--tier", "public", "--db", "private.db"])
        .output()
        .unwrap();
    assert!(!bad.status.success());
}
