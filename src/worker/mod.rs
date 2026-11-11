pub mod sandbox;
pub mod scan;
pub mod watch;

use std::error::Error;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::proto::{Job, Message};

#[derive(Clone)]
pub struct Output {
    writer: Arc<Mutex<io::Stdout>>,
    entries: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
}
impl Output {
    fn send(&self, message: &Message) -> Result<(), Box<dyn Error>> {
        let mut out = self.writer.lock().map_err(|_| "stdout poisoned")?;
        serde_json::to_writer(&mut *out, message)?;
        out.write_all(b"\n")?;
        out.flush()?;
        Ok(())
    }
    pub fn entry(&self) {
        self.entries.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn run(config: &Path, role: &str, root: Option<&str>) -> Result<(), Box<dyn Error>> {
    let mut ready = sandbox::start(config, role, root)?;
    let output = Output {
        writer: Arc::new(Mutex::new(io::stdout())),
        entries: Arc::new(AtomicU64::new(0)),
        bytes: Arc::new(AtomicU64::new(0)),
    };
    let running = Arc::new(AtomicBool::new(true));
    let progress = {
        let writer = output.writer.clone();
        let entries = output.entries.clone();
        let bytes = output.bytes.clone();
        let running = running.clone();
        std::thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(10));
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                let msg = Message::Progress {
                    entries: Some(entries.swap(0, Ordering::Relaxed)),
                    bytes: Some(bytes.swap(0, Ordering::Relaxed)),
                };
                if let Ok(mut out) = writer.lock() {
                    let _ = serde_json::to_writer(&mut *out, &msg);
                    let _ = out.write_all(b"\n");
                    let _ = out.flush();
                }
            }
        })
    };
    let result = (|| -> Result<(), Box<dyn Error>> {
        for line in io::stdin().lock().lines() {
            let job: Job = serde_json::from_str(&line?)?;
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
    })();
    running.store(false, Ordering::Relaxed);
    progress.thread().unpark();
    // The writer thread exits after its current sleep; the process may exit first.
    result
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
        assert_eq!(
            std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(ready.oom_score_adj.trim(), "1000");
        assert!(
            scan::mount_identity(&mut ready.mountinfo, Path::new(&base))
                .unwrap()
                .is_some()
        );
        let output = Output {
            writer: Arc::new(Mutex::new(io::stdout())),
            entries: Arc::new(AtomicU64::new(0)),
            bytes: Arc::new(AtomicU64::new(0)),
        };
        scan::scan(&mut ready, &output).unwrap();
    }

    #[test]
    fn sandbox_blocks_write_and_tcp() {
        let base = std::env::temp_dir().join(format!("llm-index-worker-{}", std::process::id()));
        fs::create_dir_all(base.join("root/child")).unwrap();
        fs::write(base.join("root/parent.txt"), "parent").unwrap();
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
            .output()
            .unwrap();
        fs::remove_dir_all(&base).unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let stdout = String::from_utf8_lossy(&result.stdout);
        assert!(stdout.contains("1 passed"));
        assert!(stdout.contains("parent.txt"));
        assert!(stdout.contains("\"reason\":\"symlink\""));
        assert!(!stdout.contains("nested.txt"));
    }

    #[test]
    fn validate_symlink_and_missing_tail() {
        let base =
            std::env::temp_dir().join(format!("llm-index-validation-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        std::os::unix::fs::symlink("/tmp", base.join("link")).unwrap();
        assert_eq!(scan::validate_path(&base.join("link/missing")), "eloop");
        assert_eq!(scan::validate_path(&base.join("missing/tail")), "ok");
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
        let result = sandbox::start(&base.join("config.toml"), "scan", Some("parent"));
        assert!(result.err().unwrap().to_string().contains("config_error"));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn watch_error_and_submount() {
        assert!(watch::skip_submount(
            Path::new("/root/mount/file"),
            Path::new("/root"),
            "1 0 0:1 / /root rw - ext4 /dev/x rw\n2 1 0:2 / /root/mount rw - tmpfs tmpfs rw"
        ));
        assert!(!watch::skip_submount(
            Path::new("/root/other"),
            Path::new("/root"),
            "2 1 0:2 / /root/mount rw - tmpfs tmpfs rw"
        ));
        assert!(matches!(
            watch::degraded(&io::Error::from_raw_os_error(28)),
            Some(Message::WatchDegraded { .. })
        ));
    }
}
