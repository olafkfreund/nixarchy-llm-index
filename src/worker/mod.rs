pub mod sandbox;
pub mod scan;
pub mod watch;

use std::cell::Cell;
use std::error::Error;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::proto::{Job, Message};

pub struct Output {
    writer: Mutex<io::Stdout>,
    entries: Cell<u64>,
    bytes: Cell<u64>,
    sent_entries: Cell<u64>,
    sent_bytes: Cell<u64>,
    last_progress: Cell<Instant>,
}
impl Output {
    fn new() -> Self {
        Self {
            writer: Mutex::new(io::stdout()),
            entries: Cell::new(0),
            bytes: Cell::new(0),
            sent_entries: Cell::new(0),
            sent_bytes: Cell::new(0),
            last_progress: Cell::new(Instant::now()),
        }
    }
    fn send(&self, message: &Message) -> Result<(), Box<dyn Error>> {
        let mut out = self.writer.lock().map_err(|_| "stdout poisoned")?;
        serde_json::to_writer(&mut *out, message)?;
        out.write_all(b"\n")?;
        out.flush()?;
        Ok(())
    }
    pub fn entry(&self, bytes: u64) -> Result<(), Box<dyn Error>> {
        self.entries.set(self.entries.get() + 1);
        self.bytes.set(self.bytes.get() + bytes);
        self.progress()
    }
    pub fn progress(&self) -> Result<(), Box<dyn Error>> {
        if self.last_progress.get().elapsed() >= Duration::from_secs(1)
            && (self.entries.get() > self.sent_entries.get()
                || self.bytes.get() > self.sent_bytes.get())
        {
            self.send(&Message::Progress {
                entries: Some(self.entries.get()),
                bytes: Some(self.bytes.get()),
            })?;
            self.sent_entries.set(self.entries.get());
            self.sent_bytes.set(self.bytes.get());
            self.last_progress.set(Instant::now());
        }
        Ok(())
    }
}

pub fn run(config: &Path, role: &str, root: Option<&str>) -> Result<(), Box<dyn Error>> {
    let output = Output::new();
    let mut ready = match sandbox::start(config, role, root) {
        Ok(ready) => ready,
        Err(error) => {
            let message = error.to_string();
            if let Some((state, reason)) = message.split_once(':')
                && (state == "config_error" || state == "offline")
            {
                output.send(&Message::RootError {
                    state: state.into(),
                    reason: reason.trim().into(),
                })?;
            }
            return Err(error);
        }
    };
    (|| -> Result<(), Box<dyn Error>> {
        loop {
            let mut line = String::new();
            if io::stdin().read_line(&mut line)? == 0 {
                break;
            }
            let job: Job = serde_json::from_str(&line)?;
            match job {
                Job::Shutdown => break,
                Job::Scan if role == "scan" => scan::scan(&mut ready, &output)?,
                Job::Validate { paths } if role == "validate" => scan::validate(paths, &output)?,
                Job::Watch if role == "watch" => watch::watch(&mut ready, &output)?,
                Job::Extract { relpath, .. } if role == "extract" => {
                    output.send(&Message::Failed {
                        relpath,
                        reason: "not implemented".into(),
                    })?
                }
                _ => return Err("job does not match worker role".into()),
            }
        }
        Ok(())
    })()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    #[test]
    fn sandbox_child() {
        if std::env::var_os("LLM_INDEX_SANDBOX_CHILD").is_none() {
            return;
        }
        let base = std::env::var("LLM_INDEX_TEST_BASE").unwrap();
        let mut ready = sandbox::start(
            &Path::new(&base).join("config.toml"),
            "scan",
            Some("parent"),
        )
        .unwrap();
        assert!(fs::write(Path::new(&base).join("root/new"), "bad").is_err());
        assert!(fs::read_to_string(Path::new(&base).join("config.toml")).is_err());
        assert_eq!(
            std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            std::net::TcpStream::connect("127.0.0.1:1")
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(ready.oom_score_adj.trim(), "1000");
        let inotify = inotify::Inotify::init().unwrap();
        let pinned = format!(
            "/proc/self/fd/{}/.",
            std::os::fd::AsRawFd::as_raw_fd(ready.root_fd.as_ref().unwrap())
        );
        assert!(
            inotify
                .watches()
                .add(
                    &pinned,
                    inotify::WatchMask::ONLYDIR
                        | inotify::WatchMask::DONT_FOLLOW
                        | inotify::WatchMask::CREATE
                )
                .is_ok()
        );
        assert!(
            scan::mount_identity(&mut ready.mountinfo, Path::new(&base))
                .unwrap()
                .is_some()
        );
        let output = Output::new();
        scan::scan(&mut ready, &output).unwrap();
    }

    #[test]
    fn sandbox_blocks_write_and_tcp() {
        let base = std::env::temp_dir().join(format!("llm-index-worker-{}", std::process::id()));
        fs::create_dir_all(base.join("root/child")).unwrap();
        fs::write(base.join("root/parent.txt"), "parent").unwrap();
        fs::write(base.join("root/ignored.txt"), "ignored").unwrap();
        fs::create_dir_all(base.join("config-home/git")).unwrap();
        fs::write(base.join("ignore-source"), "ignored.txt\n").unwrap();
        std::os::unix::fs::symlink(
            base.join("ignore-source"),
            base.join("config-home/git/ignore"),
        )
        .unwrap();
        fs::create_dir(base.join("root/noaccess")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            base.join("root/noaccess"),
            fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        fs::write(base.join("root/.env"), "parent.txt\n").unwrap();
        std::os::unix::fs::symlink(".env", base.join("root/.llmindexignore")).unwrap();
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            base.join("root/.ignore"),
            rustix::fs::Mode::from_bits_retain(0o600),
        )
        .unwrap();
        fs::write(base.join("root/child/nested.txt"), "nested").unwrap();
        std::os::unix::fs::symlink("parent.txt", base.join("root/link.txt")).unwrap();
        fs::write(
            base.join("config.toml"),
            format!(
                "state_dir = \"/tmp/state\"\n[[root]]\nname = \"parent\"\npath = \"{}\"\n[[root]]\nname = \"child\"\npath = \"{}\"\n",
                base.join("root").display(), base.join("root/child").display()
            ),
        )
        .unwrap();
        let result = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "worker::tests::sandbox_child", "--nocapture"])
            .env("LLM_INDEX_SANDBOX_CHILD", "1")
            .env("LLM_INDEX_TEST_BASE", &base)
            .env("XDG_CONFIG_HOME", base.join("config-home"))
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let stdout = String::from_utf8_lossy(&result.stdout);
        assert!(stdout.contains("1 passed"));
        assert!(stdout.contains("parent.txt"));
        assert!(stdout.contains("\"relpath\":\"ignored.txt\",\"reason\":\"ignored\""));
        assert!(stdout.contains("\"relpath\":\"noaccess\",\"reason\":\"EACCES\""));
        assert!(stdout.contains("\"reason\":\"symlink\""));
        assert!(stdout.contains("\"relpath\":\".llmindexignore\",\"reason\":\"symlink\""));
        assert!(stdout.contains("\"complete\":true"));
        assert!(!stdout.contains("nested.txt"));
        let done: serde_json::Value = serde_json::from_str(
            stdout
                .lines()
                .find(|line| line.starts_with("{\"message\":\"ScanDone\""))
                .expect("ScanDone message"),
        )
        .unwrap();
        assert_eq!(done["identity_before"], done["identity_after"]);
        assert!(done["identity_before"]["share"]["mount_point"].is_string());
        assert!(done["identity_before"]["mount_id"].is_number());
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "worker::tests::sandbox_child_root",
                "--nocapture",
            ])
            .env("LLM_INDEX_SANDBOX_CHILD", "1")
            .env("LLM_INDEX_TEST_BASE", &base)
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{}",
            String::from_utf8_lossy(&child.stderr)
        );
        let child_stdout = String::from_utf8_lossy(&child.stdout);
        assert_eq!(child_stdout.matches("nested.txt").count(), 1);
        fs::remove_file(base.join("root/.llmindexignore")).unwrap();
        fs::rename(base.join("root/.ignore"), base.join("root/.llmindexignore")).unwrap();
        let fifo = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "worker::tests::sandbox_child", "--nocapture"])
            .env("LLM_INDEX_SANDBOX_CHILD", "1")
            .env("LLM_INDEX_TEST_BASE", &base)
            .output()
            .unwrap();
        assert!(
            fifo.status.success(),
            "{}",
            String::from_utf8_lossy(&fifo.stderr)
        );
        assert!(String::from_utf8_lossy(&fifo.stdout).contains("1 passed"));
        fs::set_permissions(
            base.join("root/noaccess"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn sandbox_child_root() {
        if std::env::var_os("LLM_INDEX_SANDBOX_CHILD").is_none() {
            return;
        }
        let base = std::env::var("LLM_INDEX_TEST_BASE").unwrap();
        let mut ready =
            sandbox::start(&Path::new(&base).join("config.toml"), "scan", Some("child")).unwrap();
        scan::scan(&mut ready, &Output::new()).unwrap();
    }

    #[test]
    fn watch_child() {
        if std::env::var_os("LLM_INDEX_WATCH_CHILD").is_none() {
            return;
        }
        let base = std::env::var("LLM_INDEX_TEST_BASE").unwrap();
        let mut ready =
            sandbox::start(&Path::new(&base).join("config.toml"), "watch", Some("root")).unwrap();
        watch::watch(&mut ready, &Output::new()).unwrap();
    }

    #[test]
    fn watcher_closes_fds_and_watches_recreated_directory() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Stdio;
        let base = std::env::temp_dir().join(format!("llm-index-watch-{}", std::process::id()));
        fs::create_dir_all(base.join("root/recreated")).unwrap();
        for n in 0..1100 {
            fs::create_dir(base.join(format!("root/dir-{n}"))).unwrap();
        }
        // an unreadable directory must not degrade the watcher
        fs::create_dir(base.join("root/locked")).unwrap();
        fs::set_permissions(base.join("root/locked"), fs::Permissions::from_mode(0o000)).unwrap();
        fs::write(
            base.join("config.toml"),
            format!(
                "state_dir = \"/tmp/state\"\n[[root]]\nname = \"root\"\npath = \"{}\"\n",
                base.join("root").display()
            ),
        )
        .unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "worker::tests::watch_child", "--nocapture"])
            .env("LLM_INDEX_WATCH_CHILD", "1")
            .env("LLM_INDEX_TEST_BASE", &base)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();
        loop {
            let watched = fs::read_dir(format!("/proc/{}/fdinfo", child.id()))
                .unwrap()
                .filter_map(Result::ok)
                .filter_map(|entry| fs::read_to_string(entry.path()).ok())
                .map(|info| {
                    info.lines()
                        .filter(|line| line.starts_with("inotify wd:"))
                        .count()
                })
                .sum::<usize>();
            if watched >= 1102 {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "only {watched} directories watched"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let fds = fs::read_dir(format!("/proc/{}/fd", child.id()))
            .unwrap()
            .count();
        assert!(fds < 64, "watcher held {fds} fds");
        fs::remove_dir(base.join("root/recreated")).unwrap();
        fs::create_dir(base.join("root/recreated")).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        fs::write(base.join("root/recreated/new.txt"), "new").unwrap();
        // a file written into a brand-new directory before its watch exists is still reported
        fs::create_dir(base.join("root/fresh")).unwrap();
        fs::write(base.join("root/fresh/early.txt"), "early").unwrap();
        std::thread::sleep(Duration::from_secs(3));
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"{\"type\":\"Shutdown\"}\n")
            .unwrap();
        let result = child.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let stdout = String::from_utf8_lossy(&result.stdout);
        assert!(stdout.contains("recreated/new.txt"));
        assert!(stdout.contains("fresh/early.txt"), "{stdout}");
        assert!(!stdout.contains("WatchDegraded"), "{stdout}");
        fs::set_permissions(base.join("root/locked"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn validate_symlink_and_missing_tail() {
        let base =
            std::env::temp_dir().join(format!("llm-index-validation-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        std::os::unix::fs::symlink("/tmp", base.join("link")).unwrap();
        assert_eq!(
            scan::validate_path(&base.join("link/missing")),
            crate::proto::ValidationResult::Eloop
        );
        assert_eq!(
            scan::validate_path(&base.join("missing/tail")),
            crate::proto::ValidationResult::Ok
        );
        fs::write(base.join("regular"), "x").unwrap();
        assert_eq!(
            scan::validate_path(&base.join("regular/child")),
            crate::proto::ValidationResult::Ok
        );
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            base.join("pipe"),
            rustix::fs::Mode::from_bits_retain(0o600),
        )
        .unwrap();
        assert_eq!(
            scan::validate_path(&base.join("pipe")),
            crate::proto::ValidationResult::Ok
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn symlink_root_is_config_error() {
        let base = std::env::temp_dir().join(format!("llm-index-root-{}", std::process::id()));
        fs::create_dir_all(base.join("real")).unwrap();
        std::os::unix::fs::symlink(base.join("real"), base.join("link")).unwrap();
        fs::write(
            base.join("config.toml"),
            format!(
                "state_dir = \"/tmp/state\"\n[[root]]\nname = \"parent\"\npath = \"{}\"\n",
                base.join("link").display()
            ),
        )
        .unwrap();
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "worker::tests::symlink_root_child",
                "--nocapture",
            ])
            .env("LLM_INDEX_ROOT_CHILD", "1")
            .env("LLM_INDEX_TEST_BASE", &base)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let stdout = String::from_utf8_lossy(&result.stdout);
        assert!(stdout.contains("1 passed"));
        assert!(stdout.contains("\"message\":\"RootError\",\"state\":\"config_error\""));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn symlink_root_child() {
        if std::env::var_os("LLM_INDEX_ROOT_CHILD").is_none() {
            return;
        }
        let base = std::env::var("LLM_INDEX_TEST_BASE").unwrap();
        assert!(
            run(
                &Path::new(&base).join("config.toml"),
                "scan",
                Some("parent")
            )
            .is_err()
        );
    }

    #[test]
    fn watch_error_and_submount() {
        assert!(matches!(
            watch::degraded(&io::Error::from_raw_os_error(28)),
            Message::WatchDegraded { .. }
        ));
        let start = std::time::Instant::now();
        assert!(!watch::due(start, start + Duration::from_secs(1)));
        assert!(watch::due(
            start + Duration::from_secs(1),
            start + Duration::from_secs(3)
        ));
    }
}
