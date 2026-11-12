//! `llm-index worker --role watch` as a child: unsafe names must not produce events.
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[test]
fn watcher_ignores_unsafe_names_but_reports_safe_ones() {
    let base = std::env::temp_dir().join(format!("llm-index-watch-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let root = base.join("root");
    // `good` exists before the worker starts, so it is watched; the unsafe directory is
    // created after WatchReady, as a live event
    fs::create_dir_all(root.join("good")).unwrap();
    let cfg = base.join("config.toml");
    fs::write(
        &cfg,
        format!(
            "state_dir = \"{}\"\n[[root]]\nname = \"r\"\npath = \"{}\"\n",
            base.join("state").display(),
            root.display()
        ),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_llm-index"))
        .arg("--config")
        .arg(&cfg)
        .args(["worker", "--role", "watch", "--root", "r"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    writeln!(stdin, r#"{{"type":"Watch"}}"#).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut lines = Vec::new();
    let wait_for = |needle: &str, lines: &mut Vec<String>| {
        while Instant::now() < deadline {
            if let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
                let hit = line.contains(needle);
                lines.push(line);
                if hit {
                    return true;
                }
            }
        }
        false
    };
    assert!(
        wait_for("WatchReady", &mut lines),
        "no WatchReady: {lines:?}"
    );
    fs::create_dir_all(root.join("bad\u{202e}dir")).unwrap();
    fs::write(root.join("bad\u{202e}name.md"), "x").unwrap();
    fs::write(root.join("bad\u{202e}dir/ok.md"), "x").unwrap();
    fs::write(root.join("good/ok.md"), "x").unwrap();
    // the positive control proves the watcher ran past the debounce
    let seen = wait_for("good/ok.md", &mut lines);
    // keep reading for a window after the safe event, so a late unsafe event would show
    let window = Instant::now() + Duration::from_secs(3);
    while Instant::now() < window {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
            lines.push(line);
        }
    }
    drop(stdin);
    let _ = child.wait();
    let _ = fs::remove_dir_all(&base);
    assert!(seen, "no event for good/ok.md: {lines:?}");
    assert!(
        lines
            .iter()
            .all(|l| !l.contains("202e") && !l.contains('\u{202e}')),
        "{lines:?}"
    );
}
