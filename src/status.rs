//! `status`: per visible root state and counts, sandbox state, DB sizes. Shared by the MCP
//! `status` tool (`public` hides everything about private roots) and the `status` CLI.

use std::error::Error;
use std::fmt::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};

use crate::config::{self, Config, Kind, Tier};
use crate::db::{Mode, Store};
use crate::rank::visible_sql;

type Res<T> = Result<T, Box<dyn Error>>;

pub fn sandbox_state() -> &'static str {
    use landlock::{ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr};
    // Creating a ruleset enforces nothing; it only proves the kernel supports Landlock.
    let probe = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI::V1))
        .and_then(|r| r.create());
    if probe.is_ok() {
        "landlock available"
    } else {
        "landlock unavailable"
    }
}

const STATES: [&str; 4] = ["ok", "offline", "degraded", "config_error"];
const REASONS: [&str; 15] = [
    "identity mismatch",
    "identity check",
    "watch degraded",
    "watcher exited",
    "watch setup stalled",
    "stdin blocked",
    "no progress",
    "extract timeout",
    "scan timeout",
    "worker died",
    "disk full",
    "symlink in path",
    "root path contains a symlink",
    "root path is not a directory",
    "landlock unavailable",
];

/// A state word, `mount`, or an errno name such as `EACCES`.
fn public_reason(r: &str) -> bool {
    STATES.contains(&r)
        || r == "mount"
        || (r.starts_with('E')
            && r.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()))
}

fn count(conn: &Connection, sql: &str, root: &str) -> Res<i64> {
    Ok(conn.query_row(sql, [root], |r| r.get(0))?)
}

/// Call inside `store.begin()`. `public` lists public-tier roots from `public.db` only.
pub fn build(store: &Store, public: bool) -> Res<Value> {
    build_inner(store, public, false)
}

fn build_inner(store: &Store, public: bool, brief: bool) -> Res<Value> {
    let vis = visible_sql(store.mode);
    let (state_dir, roots) = store
        .with_config(|c| {
            let roots: Vec<_> = c
                .roots
                .iter()
                .filter(|r| !public || r.tier == Tier::Public)
                .map(|r| {
                    (
                        r.name.clone(),
                        r.path.to_string_lossy().into_owned(),
                        r.tier,
                    )
                })
                .collect();
            (c.state_dir.clone(), roots)
        })
        .ok_or("no config")?;
    let (files, chunks) = if brief {
        (None, None)
    } else {
        (
            Some(format!(
                "SELECT count(*) FROM files f WHERE f.root_id = ?1 AND {vis}"
            )),
            Some(format!(
                "SELECT count(*) FROM chunks c JOIN files f ON f.id = c.file_id WHERE f.root_id = ?1 AND {vis}"
            )),
        )
    };
    let mut out = Vec::new();
    for (name, path, tier) in roots {
        let mut row = json!({"name": name, "state": "pending", "files": if brief { Value::Null } else { json!(0) }, "chunks": if brief { Value::Null } else { json!(0) }});
        for conn in store.conns() {
            let found = conn
                .query_row(
                    "SELECT state, reason, last_scan, queued, done, skipped_symlinks FROM roots
                     WHERE name = ?1 AND path = ?2",
                    [&name, &path],
                    |r| {
                        Ok(json!({
                            "state": r.get::<_, Option<String>>(0)?,
                            "reason": r.get::<_, Option<String>>(1)?,
                            "last_scan": r.get::<_, Option<i64>>(2)?,
                            "queued": r.get::<_, Option<i64>>(3)?,
                            "done": r.get::<_, Option<i64>>(4)?,
                            "skipped_symlinks": r.get::<_, Option<i64>>(5)?,
                        }))
                    },
                )
                .optional()?;
            if let Some(Value::Object(mut f)) = found {
                if public {
                    // the public tier shows a fixed vocabulary only, never free text
                    let state = f["state"]
                        .as_str()
                        .filter(|s| STATES.contains(s))
                        .unwrap_or("pending");
                    f["state"] = state.into();
                    let reason = f["reason"]
                        .as_str()
                        .filter(|r| public_reason(r))
                        .map(String::from);
                    f["reason"] = reason.into();
                }
                row.as_object_mut().unwrap().extend(f);
                if let (Some(files), Some(chunks)) = (&files, &chunks) {
                    row["files"] = count(conn, files, &name)?.into();
                    row["chunks"] = count(conn, chunks, &name)?.into();
                }
                break;
            }
        }
        if !public {
            row["tier"] = if tier == Tier::Public {
                "public"
            } else {
                "private"
            }
            .into();
        }
        out.push(row);
    }
    let size = |n: &str| {
        std::fs::metadata(state_dir.join(n))
            .map(|m| m.len())
            .unwrap_or(0)
    };
    let mut dbs = vec![json!({"name": "public.db", "bytes": size("public.db")})];
    let mut result = json!({"roots": out, "sandbox": sandbox_state()});
    if let (false, Some(private)) = (public, store.private.as_ref()) {
        dbs.push(json!({"name": "private.db", "bytes": size("private.db")}));
        let mut stmt = private.prepare("SELECT path, reason FROM config_errors ORDER BY path")?;
        let errs = stmt
            .query_map([], |r| {
                Ok(json!({"path": r.get::<_, String>(0)?, "reason": r.get::<_, Option<String>>(1)?}))
            })?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|e| !crate::sanitize::unsafe_path(e["path"].as_str().unwrap_or_default()))
            .collect::<Vec<_>>();
        result["config_errors"] = errs.into();
    }
    result["databases"] = dbs.into();
    Ok(result)
}

fn safe_reason(value: &Value) -> Value {
    match value.as_str().filter(|r| !r.is_empty()) {
        None => Value::Null,
        Some(r) if public_reason(r) || REASONS.contains(&r) => json!(r),
        Some(_) => json!("other"),
    }
}

fn json(store: &Store, config: Config, brief: bool) -> Res<Value> {
    store.set_config(config);
    // stamped when the read snapshot starts: a full run's slow counts must not
    // make its (older) root states look newer than a brief run's.
    let generated_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let _snapshot = store.begin()?;
    let mut status = build_inner(store, false, brief)?;
    for row in status["roots"].as_array_mut().unwrap() {
        let state = row["state"].as_str().unwrap_or_default();
        if !STATES.contains(&state) {
            row["state"] = json!("pending");
        }
        row["reason"] = safe_reason(&row["reason"]);
        for field in ["last_scan", "queued", "done", "skipped_symlinks"] {
            if row.get(field).is_none() {
                row[field] = Value::Null;
            }
        }
    }
    store.with_config(|config| {
    let errors = status["config_errors"].as_array_mut().unwrap();
    for error in errors {
        let path = Path::new(error["path"].as_str().unwrap_or_default());
        let root = config
            .roots
            .iter()
            .filter(|r| path.starts_with(&r.path))
            .max_by_key(|r| r.path.components().count());
        let (name, what) = match root {
            Some(r) if path == r.path => (Some(r.name.as_str()), "root"),
            Some(r) => (Some(r.name.as_str()), "exclusion"),
            None => (None, "unknown"),
        };
        *error = json!({"root": name, "what": what, "reason": safe_reason(&error["reason"])});
    }
    status["settings"] = json!({
        "roots": config.roots.iter().map(|r| json!({
            "name": r.name,
            "tier": if r.tier == Tier::Public { "public" } else { "private" },
            "kind": match r.kind { Kind::Auto => "auto", Kind::Local => "local", Kind::Network => "network" },
            "interval": r.interval,
            "share": r.share.is_some(),
        })).collect::<Vec<_>>(),
        "exclusions": config.exclude.paths.len(),
        "lifted_builtin": config.exclude.lift_builtin.len(),
        "limits": {
            "max_file_mb": config.limits.max_file_mb,
            "max_text_bytes": config.limits.max_text_bytes,
            "max_cells": config.limits.max_cells,
            "memory_max_mb": config.limits.memory_max_mb,
            "workers": config.limits.workers,
            "backoff_max": config.limits.backoff_max,
        },
    });
    }).ok_or("no config")?;
    status["generated_at"] = generated_at.into();
    status["brief"] = brief.into();
    Ok(status)
}

fn show(v: &Value) -> String {
    match v {
        Value::Null => "-".into(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

/// `llm-index status`: the owner's view, private roots and clear-text config errors included.
pub fn run(config_path: &Path, as_json: bool, brief: bool) -> Res<()> {
    let config = config::load(config_path)?;
    let store = Store::open(&config.state_dir, Mode::Both)?;
    if as_json {
        let result = json(&store, config, brief)?;
        println!("{result}");
        return Ok(());
    }
    store.set_config(config);
    let _snapshot = store.begin()?;
    let status = build(&store, false)?;
    print!("{}", text(&status));
    Ok(())
}

fn text(status: &Value) -> String {
    let mut out = String::new();
    for r in status["roots"].as_array().into_iter().flatten() {
        writeln!(
            out,
            "{} [{}] {}  reason: {}  last scan: {}  queued/done: {}/{}  files: {}  chunks: {}  skipped symlinks: {}",
            show(&r["name"]),
            show(&r["tier"]),
            show(&r["state"]),
            show(&r["reason"]),
            show(&r["last_scan"]),
            show(&r["queued"]),
            show(&r["done"]),
            show(&r["files"]),
            show(&r["chunks"]),
            show(&r["skipped_symlinks"]),
        ).unwrap();
    }
    for e in status["config_errors"].as_array().into_iter().flatten() {
        writeln!(
            out,
            "config error: {}: {}",
            show(&e["path"]),
            show(&e["reason"])
        )
        .unwrap();
    }
    writeln!(out, "sandbox: {}", show(&status["sandbox"])).unwrap();
    for d in status["databases"].as_array().into_iter().flatten() {
        writeln!(out, "{}: {} bytes", show(&d["name"]), show(&d["bytes"])).unwrap();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> (std::path::PathBuf, Config) {
        let dir = std::env::temp_dir().join(format!(
            "llm-index-status-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let content = format!(
            r#"
            state_dir = {:?}
            [[root]]
            name = "pub"
            path = "/data/pub"
            tier = "public"
            [[root]]
            name = "moving"
            path = "/data/pub/moving"
            share = "secret source"
            [[root]]
            name = "a"
            path = "/data/a"
            [[root]]
            name = "checking"
            path = "/data/checking"
            [exclude]
            paths = ["/data/pub/x"]
            lift_builtin = ["secret/glob"]
        "#,
            dir.to_str().unwrap()
        );
        fs::write(dir.join("config.toml"), &content).unwrap();
        let config: Config = toml::from_str(&content).unwrap();
        let writer = Store::open(&dir, Mode::Writer).unwrap();
        writer.public.execute("INSERT INTO roots(name,path,state,reason,last_scan,queued,done,skipped_symlinks) VALUES('pub','/data/pub','', '', 12, 3, 4, 5)", []).unwrap();
        writer.private.as_ref().unwrap().execute("INSERT INTO roots(name,path,state,reason) VALUES('moving','/data/pub/moving','offline','/secret/path')", []).unwrap();
        writer.private.as_ref().unwrap().execute("INSERT INTO roots(name,path,state,reason) VALUES('checking','/data/checking','offline','identity check')", []).unwrap();
        let private = writer.private.as_ref().unwrap();
        for (path, reason) in [
            ("/data/pub", "symlink in path"),
            ("/data/pub/x", "/secret/path"),
            ("/data/pub/moving/x", "EACCES"),
            ("/data/ab", "watch degraded"),
            ("/old/root", ""),
        ] {
            private
                .execute(
                    "INSERT INTO config_errors(path,reason) VALUES(?1,?2)",
                    [path, reason],
                )
                .unwrap();
        }
        drop(writer);
        (dir, config)
    }

    #[test]
    fn json_contract_and_golden() {
        let (dir, config) = fixture();
        let store = Store::open(&dir, Mode::Both).unwrap();
        store.set_config(config);
        let _snapshot = store.begin().unwrap();
        let mut original = build(&store, false).unwrap();
        original["sandbox"] = json!("landlock available");
        for db in original["databases"].as_array_mut().unwrap() {
            assert!(db["bytes"].as_u64().unwrap() > 0);
            db["bytes"] = json!(1);
        }
        assert_eq!(
            text(&original),
            concat!(
                "pub [public]   reason:   last scan: 12  queued/done: 3/4  files: 0  chunks: 0  skipped symlinks: 5\n",
                "moving [private] offline  reason: /secret/path  last scan: -  queued/done: -/-  files: 0  chunks: 0  skipped symlinks: -\n",
                "a [private] pending  reason: -  last scan: -  queued/done: -/-  files: 0  chunks: 0  skipped symlinks: -\n",
                "checking [private] offline  reason: identity check  last scan: -  queued/done: -/-  files: 0  chunks: 0  skipped symlinks: -\n",
                "config error: /data/ab: watch degraded\n",
                "config error: /data/pub: symlink in path\n",
                "config error: /data/pub/moving/x: EACCES\n",
                "config error: /data/pub/x: /secret/path\n",
                "config error: /old/root: \n",
                "sandbox: landlock available\n",
                "public.db: 1 bytes\n",
                "private.db: 1 bytes\n",
            )
        );
        drop(_snapshot);
        for (brief, file) in [
            (false, "plugin/tests/fixtures/status-full.json"),
            (true, "plugin/tests/fixtures/status-brief.json"),
        ] {
            let mut value = json(
                &store,
                config::load(&dir.join("config.toml")).unwrap(),
                brief,
            )
            .unwrap();
            value["generated_at"] = json!(0);
            value["sandbox"] = json!("landlock available");
            for db in value["databases"].as_array_mut().unwrap() {
                assert!(db["bytes"].as_u64().unwrap() > 0);
                db["bytes"] = json!(1);
            }
            let roots = value["roots"].as_array().unwrap().clone();
            assert_eq!(roots.len(), 4);
            assert_eq!(roots[0]["state"], "pending");
            assert!(roots[0]["reason"].is_null());
            assert_eq!(roots[1]["reason"], "other");
            assert!(roots[2]["last_scan"].is_null());
            assert_eq!(roots[2]["state"], "pending");
            assert_eq!(roots[3]["state"], "offline");
            assert_eq!(roots[3]["reason"], "identity check");
            assert_eq!(roots[0]["files"].is_null(), brief);
            assert_eq!(roots[0]["chunks"].is_null(), brief);
            let errors = value["config_errors"].as_array().unwrap().clone();
            assert_eq!(errors[0]["what"], "unknown");
            assert_eq!(errors[1]["what"], "root");
            assert_eq!(errors[2]["root"], "moving");
            assert_eq!(errors[2]["what"], "exclusion");
            assert_eq!(errors[3]["root"], "pub");
            assert_eq!(errors[3]["reason"], "other");
            assert_eq!(errors[4]["what"], "unknown");
            assert_eq!(value["settings"]["lifted_builtin"], 1);
            assert_eq!(value["settings"]["roots"][1]["share"], true);
            assert!(!value["settings"].to_string().contains('/'));
            for error in &errors {
                assert!(!error.to_string().contains('/'));
            }
            for root in &roots {
                assert!(!root["reason"].to_string().contains('/'));
            }
            let text = format!("{}\n", serde_json::to_string_pretty(&value).unwrap());
            if std::env::var_os("UPDATE_STATUS_GOLDEN").is_some() {
                fs::create_dir_all("plugin/tests/fixtures").unwrap();
                fs::write(file, &text).unwrap();
            } else {
                assert_eq!(text, fs::read_to_string(file).unwrap());
            }
        }
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn brief_never_prepares_count_queries() {
        let (dir, config) = fixture();
        let writer = Store::open(&dir, Mode::Writer).unwrap();
        writer
            .public
            .execute_batch("ALTER TABLE chunks RENAME TO chunks_x")
            .unwrap();
        drop(writer);
        let store = Store::open(&dir, Mode::Both).unwrap();
        assert!(json(&store, config, true).is_ok());
        assert!(
            json(
                &store,
                config::load(&dir.join("config.toml")).unwrap(),
                false
            )
            .is_err()
        );
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }
}
