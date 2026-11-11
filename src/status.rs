//! `status`: per visible root state and counts, sandbox state, DB sizes. Shared by the MCP
//! `status` tool (`public` hides everything about private roots) and the `status` CLI.

use std::error::Error;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};

use crate::config::{self, Tier};
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

fn count(conn: &Connection, sql: &str, root: &str) -> Res<i64> {
    Ok(conn.query_row(sql, [root], |r| r.get(0))?)
}

/// Call inside `store.begin()`. `public` lists public-tier roots from `public.db` only.
pub fn build(store: &Store, public: bool) -> Res<Value> {
    let vis = visible_sql(store.mode);
    let (state_dir, roots) = store
        .with_config(|c| {
            let roots: Vec<_> = c
                .roots
                .iter()
                .filter(|r| !public || r.tier == Tier::Public)
                .map(|r| (r.name.clone(), r.tier))
                .collect();
            (c.state_dir.clone(), roots)
        })
        .ok_or("no config")?;
    let files = format!("SELECT count(*) FROM files f WHERE f.root_id = ?1 AND {vis}");
    let chunks = format!(
        "SELECT count(*) FROM chunks c JOIN files f ON f.id = c.file_id WHERE f.root_id = ?1 AND {vis}"
    );
    let mut out = Vec::new();
    for (name, tier) in roots {
        let mut row = json!({"name": name, "state": "pending", "files": 0, "chunks": 0});
        for conn in store.conns() {
            let found = conn
                .query_row(
                    "SELECT state, reason, last_scan, queued, done, skipped_symlinks FROM roots WHERE name = ?1",
                    [&name],
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
            if let Some(Value::Object(f)) = found {
                row.as_object_mut().unwrap().extend(f);
                row["files"] = count(conn, &files, &name)?.into();
                row["chunks"] = count(conn, &chunks, &name)?.into();
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
            .collect::<Result<Vec<_>, _>>()?;
        result["config_errors"] = errs.into();
    }
    result["databases"] = dbs.into();
    Ok(result)
}

fn show(v: &Value) -> String {
    match v {
        Value::Null => "-".into(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

/// `llm-index status`: the owner's view, private roots and clear-text config errors included.
pub fn run(config_path: &Path) -> Res<()> {
    let config = config::load(config_path)?;
    let store = Store::open(&config.state_dir, Mode::Both)?;
    store.set_config(config);
    let _snapshot = store.begin()?;
    let status = build(&store, false)?;
    for r in status["roots"].as_array().into_iter().flatten() {
        println!(
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
        );
    }
    for e in status["config_errors"].as_array().into_iter().flatten() {
        println!("config error: {}: {}", show(&e["path"]), show(&e["reason"]));
    }
    println!("sandbox: {}", show(&status["sandbox"]));
    for d in status["databases"].as_array().into_iter().flatten() {
        println!("{}: {} bytes", show(&d["name"]), show(&d["bytes"]));
    }
    Ok(())
}
