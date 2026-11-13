//! The coordinator's `Sink` over `db::Store(Writer)`: tier routing, embedding, deletion
//! rules, purge and validation records. The only code that writes the databases.
use std::collections::HashSet;
use std::error::Error;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::limits::Limit;
use rusqlite::{Connection, params};

use super::{ExtractedFile, ScanResult, Sink};
use crate::auth::{self, RootStates, Validated, path_hash};
use crate::config::{self, Config, Tier};
use crate::db::{Mode, Store};
use crate::embed::{self, Embedder};
use crate::proto::{Chunk, Share, ValidationResult};

type Res<T> = Result<T, Box<dyn Error>>;

const TIERS: [Tier; 2] = [Tier::Public, Tier::Private];

fn label(t: Tier) -> &'static str {
    if t == Tier::Public {
        "public"
    } else {
        "private"
    }
}

struct FileRow {
    root_id: String,
    relpath: String,
    abspath: String,
    title: Option<String>,
    ty: Option<String>,
    size: i64,
    mtime_ns: i64,
    blake3: Option<Vec<u8>>,
    state: &'static str,
    reason: Option<String>,
}

struct ChunkRow {
    ord: i64,
    kind: String,
    start: i64,
    end: i64,
    decl_start: Option<i64>,
    decl_end: Option<i64>,
    heading_or_symbol: Option<String>,
    text: String,
    vec: Option<Vec<u8>>,
}

pub struct DbSink {
    store: Store,
    embedder: Option<Embedder>,
    config: Config,
    #[cfg(test)]
    abort_after_first_commit: bool,
    #[cfg(test)]
    embed_calls: std::cell::Cell<usize>,
    #[cfg(test)]
    move_batches: std::cell::RefCell<Vec<usize>>,
}

/// Files moved between tiers per transaction pair.
const MOVE_BATCH: usize = 256;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn log<T>(r: Res<T>) {
    if let Err(e) = r {
        eprintln!("store: {e}");
    }
}

fn unhex(s: &str) -> Vec<u8> {
    if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        (0..32)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap_or(0))
            .collect()
    } else {
        s.as_bytes().to_vec()
    }
}

/// A skip whose subtree must keep its rows: a mount boundary or an I/O error, not a rule.
fn protects(reason: &str) -> bool {
    reason == "mount"
        || reason.starts_with("Errno")
        || (reason.starts_with('E')
            && reason
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()))
}

fn under_any(rel: &str, set: &HashSet<&str>) -> bool {
    set.contains(rel) || rel.match_indices('/').any(|(i, _)| set.contains(&rel[..i]))
}

fn delete_files(c: &Connection, ids: &[i64]) -> Res<()> {
    for id in ids {
        c.execute(
            "DELETE FROM chunks_fts WHERE rowid IN (SELECT id FROM chunks WHERE file_id = ?1)",
            [id],
        )?;
        c.execute("DELETE FROM chunks WHERE file_id = ?1", [id])?;
        c.execute("DELETE FROM files_fts WHERE rowid = ?1", [id])?;
        c.execute("DELETE FROM files WHERE id = ?1", [id])?;
    }
    Ok(())
}

fn insert_file(c: &Connection, f: &FileRow, chunks: &[ChunkRow]) -> Res<()> {
    c.execute(
        "INSERT INTO files(root_id, relpath, abspath, title, type, size, mtime_ns, blake3, state, reason)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            f.root_id, f.relpath, f.abspath, f.title, f.ty, f.size, f.mtime_ns, f.blake3, f.state,
            f.reason
        ],
    )?;
    if f.state != "ok" {
        return Ok(()); // a failed file has nothing to search
    }
    let fid = c.last_insert_rowid();
    c.execute(
        "INSERT INTO files_fts(rowid, title, path) VALUES(?1, ?2, ?3)",
        params![fid, f.title, f.abspath],
    )?;
    for ch in chunks {
        c.execute(
            "INSERT INTO chunks(file_id, ord, kind, start, end, decl_start, decl_end, heading_or_symbol, text, vec)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                fid, ch.ord, ch.kind, ch.start, ch.end, ch.decl_start, ch.decl_end,
                ch.heading_or_symbol, ch.text, ch.vec
            ],
        )?;
        c.execute(
            "INSERT INTO chunks_fts(rowid, text) VALUES(?1, ?2)",
            params![c.last_insert_rowid(), ch.text],
        )?;
    }
    Ok(())
}

fn existing(c: &Connection, root: &str, rel: &str) -> Res<Vec<i64>> {
    let mut stmt = c.prepare("SELECT id FROM files WHERE root_id = ?1 AND relpath = ?2")?;
    Ok(stmt
        .query_map(params![root, rel], |r| r.get(0))?
        .collect::<Result<_, _>>()?)
}

fn read_file(c: &Connection, id: i64) -> Res<(FileRow, Vec<ChunkRow>)> {
    let f = c.query_row(
        "SELECT root_id, relpath, abspath, title, type, size, mtime_ns, blake3, state, reason
         FROM files WHERE id = ?1",
        [id],
        |r| {
            let state: String = r.get(8)?;
            Ok(FileRow {
                root_id: r.get(0)?,
                relpath: r.get(1)?,
                abspath: r.get(2)?,
                title: r.get(3)?,
                ty: r.get(4)?,
                size: r.get(5)?,
                mtime_ns: r.get(6)?,
                blake3: r.get(7)?,
                state: if state == "ok" { "ok" } else { "failed" },
                reason: r.get(9)?,
            })
        },
    )?;
    let mut stmt = c.prepare(
        "SELECT ord, kind, start, end, decl_start, decl_end, heading_or_symbol, text, vec
         FROM chunks WHERE file_id = ?1 ORDER BY ord",
    )?;
    let chunks = stmt
        .query_map([id], |r| {
            Ok(ChunkRow {
                ord: r.get(0)?,
                kind: r.get(1)?,
                start: r.get(2)?,
                end: r.get(3)?,
                decl_start: r.get(4)?,
                decl_end: r.get(5)?,
                heading_or_symbol: r.get(6)?,
                text: r.get(7)?,
                vec: r.get(8)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok((f, chunks))
}

impl DbSink {
    /// Open both databases for writing; the embedder is optional (no model → words search only).
    pub fn open(state_dir: &Path, config_path: &Path) -> Res<Self> {
        let embedder = match crate::model_dir() {
            "unset" => {
                eprintln!("store: no model directory baked in; indexing without vectors");
                None
            }
            dir => match Embedder::open(Path::new(dir), embed::Mode::Bulk) {
                Ok(e) => Some(e),
                Err(e) => {
                    eprintln!("store: embedder unavailable ({e}); indexing without vectors");
                    None
                }
            },
        };
        Self::open_with(state_dir, config_path, embedder)
    }

    pub fn open_with(
        state_dir: &Path,
        config_path: &Path,
        embedder: Option<Embedder>,
    ) -> Res<Self> {
        let store = Store::open(state_dir, Mode::Writer)?;
        store.set_config(config::load(config_path)?);
        let mut s = Self {
            store,
            embedder,
            config: config::load(config_path)?,
            #[cfg(test)]
            abort_after_first_commit: false,
            #[cfg(test)]
            embed_calls: Default::default(),
            #[cfg(test)]
            move_batches: Default::default(),
        };
        for t in TIERS {
            let tx = s.conn(t).unchecked_transaction()?;
            tx.execute(
                "INSERT INTO meta SELECT ?1, 1 WHERE NOT EXISTS (SELECT 1 FROM meta)",
                [embed::MODEL_REV],
            )?;
            // A new model makes every stored vector wrong: drop them and re-extract files.
            let stale: bool = tx.query_row(
                "SELECT coalesce(model_rev <> ?1, 0) FROM meta",
                [embed::MODEL_REV],
                |r| r.get(0),
            )?;
            if stale {
                tx.execute("UPDATE chunks SET vec = NULL", [])?;
                tx.execute("UPDATE files SET size = -1 WHERE state = 'ok'", [])?;
                tx.execute("UPDATE meta SET model_rev = ?1", [embed::MODEL_REV])?;
            }
            tx.commit()?;
        }
        s.sync_roots()?;
        s.purge()?;
        s.compact();
        Ok(s)
    }

    /// New config: re-sync the roots tables and drop or move rows it no longer allows.
    pub fn reload(&mut self, config_path: &Path) -> Res<()> {
        let (a, b) = (config::load(config_path)?, config::load(config_path)?);
        self.store.set_config(a);
        self.config = b;
        self.sync_roots()?;
        self.purge()?;
        self.compact();
        Ok(())
    }

    /// Give freed pages back to the filesystem when over a quarter of a tier is free.
    /// Errors are logged, never returned: a database that stays large is not an outage.
    pub fn compact(&self) {
        for t in TIERS {
            let c = self.conn(t);
            let l = label(t);
            let pragma = |p: &str| c.query_row(&format!("PRAGMA {p}"), [], |r| r.get::<_, i64>(0));
            let (pages, free) = match (pragma("page_count"), pragma("freelist_count")) {
                (Ok(p), Ok(f)) => (p, f),
                (Err(e), _) | (_, Err(e)) => {
                    eprintln!("store: compact {l}: {e}");
                    continue;
                }
            };
            if free * 4 <= pages {
                continue;
            }
            // VACUUM attaches a temp database internally; the connection normally forbids ATTACH.
            let _ = c.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 1);
            let r = c.execute_batch("VACUUM");
            let _ = c.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0);
            if let Err(e) = r {
                eprintln!("store: compact {l}: {e}");
                continue;
            }
            match c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            }) {
                Ok((1, _, _)) => eprintln!("store: compact {l}: WAL truncation deferred"),
                Ok(_) => {}
                Err(e) => eprintln!("store: compact {l}: {e}"),
            }
            let after = pragma("page_count").unwrap_or(-1);
            eprintln!("store: compacted {l}: {pages} -> {after} pages");
        }
    }

    fn conn(&self, t: Tier) -> &Connection {
        match t {
            Tier::Public => &self.store.public,
            Tier::Private => self.store.private.as_ref().expect("writer opens both"),
        }
    }

    fn root(&self, name: &str) -> Option<&config::Root> {
        self.config.roots.iter().find(|r| r.name == name)
    }

    fn visible(&self, abspath: &str) -> Option<Tier> {
        auth::visible(
            abspath,
            &self.config,
            &RootStates::new(),
            &Validated::default(),
        )
    }

    /// `public.db.roots` holds only public roots; private roots live only in `private.db`.
    /// A root that changes tier keeps its recorded share identity while its path is unchanged;
    /// a changed path clears it so the next scan derives it again.
    fn sync_roots(&self) -> Res<()> {
        let mut old: std::collections::HashMap<String, (Option<String>, Option<String>)> =
            Default::default();
        for t in TIERS {
            let mut stmt = self
                .conn(t)
                .prepare("SELECT name, path, share_id FROM roots")?;
            for r in stmt.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))? {
                let (name, v) = r?;
                old.insert(name, v);
            }
        }
        for t in TIERS {
            let c = self.conn(t);
            let mine: Vec<_> = self.config.roots.iter().filter(|r| r.tier == t).collect();
            let known: Vec<String> = c
                .prepare("SELECT name FROM roots")?
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            for name in known {
                if !mine.iter().any(|r| r.name == name) {
                    c.execute("DELETE FROM roots WHERE name = ?1", [name])?;
                }
            }
            for r in mine {
                let path = r.path.to_string_lossy();
                let carried = old
                    .get(&r.name)
                    .filter(|(p, _)| p.as_deref() == Some(&*path))
                    .and_then(|(_, id)| id.clone());
                c.execute(
                    "INSERT INTO roots(name, path, kind, share_id, state, queued, done, skipped_symlinks)
                     VALUES(?1, ?2, ?3, ?4, '', 0, 0, 0)
                     ON CONFLICT(name) DO UPDATE SET kind = excluded.kind,
                       share_id = CASE WHEN path IS excluded.path THEN share_id ELSE NULL END,
                       path = excluded.path",
                    params![
                        r.name,
                        path,
                        format!("{:?}", r.kind).to_lowercase(),
                        carried
                    ],
                )?;
            }
        }
        Ok(())
    }

    /// Drop rows of roots no longer configured, and validation records of paths no longer
    /// validated (root paths and exclusions under them).
    fn prune(&self) -> Res<()> {
        let paths: HashSet<String> = super::validate::paths(&self.config)
            .into_iter()
            .flatten()
            .collect();
        let hashes: HashSet<Vec<u8>> = paths
            .iter()
            .filter_map(|p| path_hash(Path::new(p)))
            .map(|h| h.as_bytes().to_vec())
            .collect();
        for t in TIERS {
            let c = self.conn(t);
            let orphans: Vec<i64> = c
                .prepare("SELECT id, root_id, abspath FROM files")?
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                // rows of unconfigured roots, and legacy rows with an unsafe path
                .filter(|(_, root, abs)| {
                    self.root(root).is_none() || crate::sanitize::unsafe_path(abs)
                })
                .map(|(id, _, _)| id)
                .collect();
            let stale: Vec<Vec<u8>> = c
                .prepare("SELECT path_blake3 FROM validated")?
                .query_map([], |r| r.get::<_, Vec<u8>>(0))?
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .filter(|h| !hashes.contains(h))
                .collect();
            let tx = c.unchecked_transaction()?;
            delete_files(&tx, &orphans)?;
            for h in stale {
                tx.execute("DELETE FROM validated WHERE path_blake3 = ?1", [h])?;
            }
            if t == Tier::Private {
                let bad: Vec<String> = tx
                    .prepare("SELECT path FROM config_errors")?
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .filter(|p| !paths.contains(p))
                    .collect();
                for p in bad {
                    tx.execute("DELETE FROM config_errors WHERE path = ?1", [p])?;
                }
            }
            tx.commit()?;
        }
        Ok(())
    }

    /// Remove rows `visible` now rejects; move rows whose tier changed (public delete first).
    pub fn purge(&mut self) -> Res<()> {
        self.prune()?;
        for t in TIERS {
            let c = self.conn(t);
            let ids = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> Res<Vec<i64>> {
                Ok(c.prepare(sql)?
                    .query_map(p, |r| r.get(0))?
                    .collect::<Result<_, _>>()?)
            };
            let gone = ids("SELECT id FROM files WHERE visible(abspath) IS NULL", &[])?;
            if !gone.is_empty() {
                let tx = c.unchecked_transaction()?;
                delete_files(&tx, &gone)?;
                tx.commit()?;
            }
            let other = if t == Tier::Public {
                Tier::Private
            } else {
                Tier::Public
            };
            let moved = ids(
                "SELECT id FROM files WHERE visible(abspath) = ?1",
                &[&label(other)],
            )?;
            if !moved.is_empty() {
                self.move_tier(t, other, &moved)?;
            }
        }
        Ok(())
    }

    /// One transaction per database; the source delete commits before the destination insert,
    /// so a crash in between loses a row (re-indexed later) and never leaves a copy behind.
    fn move_tier(&self, from: Tier, to: Tier, ids: &[i64]) -> Res<()> {
        for batch in ids.chunks(MOVE_BATCH) {
            #[cfg(test)]
            self.move_batches.borrow_mut().push(batch.len());
            let rows = batch
                .iter()
                .map(|id| read_file(self.conn(from), *id))
                .collect::<Res<Vec<_>>>()?;
            let tx = self.conn(from).unchecked_transaction()?;
            delete_files(&tx, batch)?;
            tx.commit()?;
            #[cfg(test)]
            if self.abort_after_first_commit {
                return Err("simulated crash after the first commit".into());
            }
            let tx = self.conn(to).unchecked_transaction()?;
            for (f, chunks) in &rows {
                delete_files(&tx, &existing(&tx, &f.root_id, &f.relpath)?)?;
                insert_file(&tx, f, chunks)?;
            }
            tx.commit()?;
        }
        Ok(())
    }

    fn vector(&self, input: &str) -> Option<Vec<u8>> {
        #[cfg(test)]
        self.embed_calls.set(self.embed_calls.get() + 1);
        let v = self.embedder.as_ref()?.embed(input).ok()??;
        Some(v.iter().map(|x| *x as u8).collect())
    }

    fn store_file(
        &self,
        root: &str,
        rel: &str,
        f: impl FnOnce(&str) -> Option<(FileRow, Vec<Chunk>)>,
    ) -> Res<()> {
        let Some(r) = self.root(root) else {
            return Ok(());
        };
        let abs = r.path.join(rel).to_string_lossy().into_owned();
        let tier = self.visible(&abs);
        // a file that moved tiers leaves the other database first (public before private)
        for other in TIERS.into_iter().filter(|o| Some(*o) != tier) {
            let c = self.conn(other);
            let ids = existing(c, root, rel)?;
            if !ids.is_empty() {
                let tx = c.unchecked_transaction()?;
                delete_files(&tx, &ids)?;
                tx.commit()?;
            }
        }
        let Some(t) = tier else {
            return Ok(()); // denied or excluded: never stored
        };
        let Some((row, chunks)) = f(&abs) else {
            return Ok(());
        };
        let c = self.conn(t);
        let tx = c.unchecked_transaction()?;
        let old = existing(&tx, root, rel)?;
        // vectors of unchanged chunks survive an edit; only new text is embedded
        let mut kept: std::collections::HashMap<(Option<String>, String), Vec<u8>> =
            Default::default();
        for id in &old {
            let mut stmt = tx.prepare(
                "SELECT heading_or_symbol, text, vec FROM chunks WHERE file_id = ?1 AND vec IS NOT NULL",
            )?;
            for r in stmt.query_map([id], |r| Ok(((r.get(0)?, r.get(1)?), r.get(2)?)))? {
                let (k, v) = r?;
                kept.insert(k, v);
            }
        }
        delete_files(&tx, &old)?;
        let rows: Vec<ChunkRow> = chunks
            .into_iter()
            .map(|ch| {
                let key = (ch.heading_or_symbol.clone(), ch.text.clone());
                let vec = kept.get(&key).cloned().or_else(|| {
                    self.vector(&embed::input(
                        rel,
                        ch.heading_or_symbol.as_deref(),
                        &ch.text,
                    ))
                });
                ChunkRow {
                    vec,
                    ord: ch.ord as i64,
                    kind: ch.kind,
                    start: ch.start as i64,
                    end: ch.end as i64,
                    decl_start: ch.decl_start.map(|x| x as i64),
                    decl_end: ch.decl_end.map(|x| x as i64),
                    heading_or_symbol: ch.heading_or_symbol,
                    text: ch.text,
                }
            })
            .collect();
        insert_file(&tx, &row, &rows)?;
        tx.execute(
            "UPDATE roots SET done = coalesce(done, 0) + 1, queued = max(coalesce(queued, 0) - 1, 0) WHERE name = ?1",
            [root],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn apply_scan(&mut self, name: &str, scan: &ScanResult) -> Res<()> {
        let Some(tier) = self.root(name).map(|r| r.tier) else {
            return Ok(());
        };
        let files: Vec<_> = scan.entries.iter().filter(|e| !e.is_dir).collect();
        if scan.complete {
            let present: HashSet<&str> = files.iter().map(|e| e.relpath.as_str()).collect();
            let protected: HashSet<&str> = scan
                .skipped
                .iter()
                .filter(|(_, why)| protects(why))
                .map(|(p, _)| p.as_str())
                .collect();
            for t in TIERS {
                let c = self.conn(t);
                let rows: Vec<(i64, String)> = c
                    .prepare("SELECT id, relpath FROM files WHERE root_id = ?1")?
                    .query_map([name], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<Result<_, _>>()?;
                let gone: Vec<i64> = rows
                    .into_iter()
                    .filter(|(_, rel)| {
                        !present.contains(rel.as_str()) && !under_any(rel, &protected)
                    })
                    .map(|(id, _)| id)
                    .collect();
                if !gone.is_empty() {
                    let tx = c.unchecked_transaction()?;
                    delete_files(&tx, &gone)?;
                    tx.commit()?;
                }
            }
        }
        let done = files
            .iter()
            .filter(|e| self.known(name, &e.relpath) == Some((e.size, e.mtime_ns)))
            .count() as i64;
        let symlinks = scan
            .skipped
            .iter()
            .filter(|(_, why)| why == "symlink")
            .count() as i64;
        self.conn(tier).execute(
            "UPDATE roots SET queued = ?2, done = ?3, skipped_symlinks = ?4,
               last_scan = CASE WHEN ?5 THEN ?6 ELSE last_scan END WHERE name = ?1",
            params![
                name,
                files.len() as i64 - done,
                done,
                symlinks,
                scan.complete,
                now_secs()
            ],
        )?;
        if scan.complete {
            self.purge()?;
        }
        Ok(())
    }
}

impl Sink for DbSink {
    fn indexed_files(&self, root: &str) -> usize {
        // 0 or 1 is enough for the verdict; a database error answers 1 so adoption stays conservative
        let any = |t: &Tier| {
            self.conn(*t)
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM files WHERE root_id = ?1 AND state = 'ok')",
                    [root],
                    |r| r.get::<_, bool>(0),
                )
                .unwrap_or(true)
        };
        usize::from(TIERS.iter().any(any))
    }

    fn expected_identity(&self, root: &str) -> Option<Share> {
        let tier = self.root(root)?.tier;
        let json: Option<String> = self
            .conn(tier)
            .query_row("SELECT share_id FROM roots WHERE name = ?1", [root], |r| {
                r.get(0)
            })
            .ok()?;
        serde_json::from_str(&json?).ok()
    }

    fn set_expected_identity(&mut self, root: &str, share: &Share) {
        let Some(tier) = self.root(root).map(|r| r.tier) else {
            return;
        };
        let json = serde_json::to_string(share).unwrap_or_default();
        log(self
            .conn(tier)
            .execute(
                "UPDATE roots SET share_id = ?2 WHERE name = ?1",
                params![root, json],
            )
            .map(drop)
            .map_err(Into::into));
    }

    fn known(&self, root: &str, relpath: &str) -> Option<(u64, i64)> {
        TIERS.iter().find_map(|t| {
            self.conn(*t)
                .query_row(
                    "SELECT size, mtime_ns FROM files WHERE root_id = ?1 AND relpath = ?2",
                    params![root, relpath],
                    |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)?)),
                )
                .ok()
        })
    }

    fn scan_done(&mut self, root: &str, scan: ScanResult) {
        let r = self.apply_scan(root, &scan);
        log(r);
    }

    fn extracted(&mut self, root: &str, f: ExtractedFile) -> Result<(), String> {
        let rel = f.relpath.clone();
        let r = self.store_file(root, &rel, |abs| {
            let title = if f.title.is_empty() {
                Path::new(&f.relpath)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            } else {
                Some(f.title.clone())
            };
            let row = FileRow {
                root_id: root.into(),
                relpath: f.relpath.clone(),
                abspath: abs.into(),
                title,
                ty: Some(f.r#type.clone()),
                size: f.size as i64,
                mtime_ns: f.mtime_ns,
                blake3: Some(unhex(&f.blake3)),
                state: "ok",
                reason: None,
            };
            Some((row, f.chunks.clone()))
        });
        r.map_err(|e| e.to_string())
    }

    fn failed(
        &mut self,
        root: &str,
        relpath: &str,
        size: u64,
        mtime_ns: i64,
        reason: &str,
    ) -> Result<(), String> {
        let r = self.store_file(root, relpath, |abs| {
            let row = FileRow {
                root_id: root.into(),
                relpath: relpath.into(),
                abspath: abs.into(),
                title: None,
                ty: None,
                size: size as i64,
                mtime_ns,
                blake3: None,
                state: "failed",
                reason: Some(reason.into()),
            };
            Some((row, Vec::new()))
        });
        r.map_err(|e| e.to_string())
    }

    fn root_state(&mut self, root: &str, state: &str, reason: &str) {
        let Some(tier) = self.root(root).map(|r| r.tier) else {
            return;
        };
        log(self
            .conn(tier)
            .execute(
                "UPDATE roots SET state = ?2, reason = ?3 WHERE name = ?1",
                params![root, state, reason],
            )
            .map(drop)
            .map_err(Into::into));
    }

    fn validated(&mut self, path: &str, result: ValidationResult) {
        let Some(hash) = path_hash(Path::new(path)) else {
            return;
        };
        let err = match result {
            ValidationResult::Ok => 0,
            ValidationResult::Eloop => 1,
            ValidationResult::Pending => return,
        };
        let r = (|| -> Res<()> {
            for t in TIERS {
                self.conn(t).execute(
                    "INSERT OR REPLACE INTO validated VALUES(?1, ?2)",
                    params![hash.as_bytes().as_slice(), err],
                )?;
            }
            // clear-text paths exist only in the private database
            let private = self.conn(Tier::Private);
            if err == 1 {
                private.execute(
                    "INSERT OR REPLACE INTO config_errors VALUES(?1, 'symlink in path')",
                    [path],
                )?;
            } else {
                private.execute("DELETE FROM config_errors WHERE path = ?1", [path])?;
            }
            Ok(())
        })();
        log(r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::tmp;
    use crate::index::Entry;
    use std::fs;

    struct Fx {
        dir: std::path::PathBuf,
        cfg: std::path::PathBuf,
        sink: DbSink,
    }

    fn write_cfg(fx_dir: &Path, tier_b: &str) -> std::path::PathBuf {
        let cfg = fx_dir.join("config.toml");
        fs::write(
            &cfg,
            format!(
                "state_dir = \"{}\"\n[[root]]\nname = \"pub\"\npath = \"/data/pub\"\ntier = \"public\"\n\
                 [[root]]\nname = \"secret\"\npath = \"/data/secret-things\"\ntier = \"{tier_b}\"\n\
                 [[root]]\nname = \"moving\"\npath = \"/data/pub/moving\"\ntier = \"public\"\n",
                fx_dir.join("state").display()
            ),
        )
        .unwrap();
        cfg
    }

    fn fx(name: &str) -> Fx {
        let dir = tmp(name);
        fs::create_dir_all(dir.join("state")).unwrap();
        let cfg = write_cfg(&dir, "private");
        let sink = DbSink::open_with(&dir.join("state"), &cfg, None).unwrap();
        Fx { dir, cfg, sink }
    }

    fn extracted(rel: &str, text: &str) -> ExtractedFile {
        ExtractedFile {
            relpath: rel.into(),
            title: format!("title of {rel}"),
            r#type: "text".into(),
            blake3: "00".repeat(32),
            chunks: vec![Chunk {
                ord: 0,
                kind: "text".into(),
                start: 0,
                end: text.len(),
                decl_start: None,
                decl_end: None,
                heading_or_symbol: None,
                text: text.into(),
            }],
            size: 5,
            mtime_ns: 7,
        }
    }

    fn big(i: usize) -> String {
        (0..80)
            .map(|l| {
                format!(
                    "word{i}x{l} lorem ipsum dolor sit amet {}",
                    i * 7919 + l * 104729
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn cfg_with(f: &Fx, roots: &[(&str, &str)]) {
        let mut t = format!("state_dir = \"{}\"\n", f.dir.join("state").display());
        for (n, p) in roots {
            t += &format!("[[root]]\nname = \"{n}\"\npath = \"{p}\"\ntier = \"public\"\n");
        }
        fs::write(&f.cfg, t).unwrap();
    }

    fn pages(f: &Fx) -> (i64, i64) {
        let c = f.sink.conn(Tier::Public);
        (
            count(c, "PRAGMA page_count"),
            count(c, "PRAGMA freelist_count"),
        )
    }

    #[test]
    fn removing_a_root_compacts_and_keeps_attach_forbidden() {
        let mut f = fx("compact");
        cfg_with(&f, &[("pub", "/data/pub"), ("keep", "/data/keep")]);
        f.sink.reload(&f.cfg).unwrap();
        for i in 0..300 {
            f.sink
                .extracted("pub", extracted(&format!("n{i}.txt"), &big(i)))
                .unwrap();
        }
        let before = pages(&f).0;
        cfg_with(&f, &[("keep", "/data/keep")]);
        f.sink.reload(&f.cfg).unwrap();
        let (after, free) = pages(&f);
        assert_eq!(free, 0);
        assert!(after < before, "{after} !< {before}");
        assert!(
            f.sink
                .conn(Tier::Public)
                .execute_batch("ATTACH ':memory:' AS x")
                .is_err()
        );
    }

    #[test]
    fn small_free_space_is_not_compacted() {
        let mut f = fx("nocompact");
        cfg_with(&f, &[("pub", "/data/pub"), ("pub2", "/data/pub2")]);
        f.sink.reload(&f.cfg).unwrap();
        for i in 0..100 {
            f.sink
                .extracted("pub", extracted(&format!("n{i}.txt"), &big(i)))
                .unwrap();
        }
        for i in 0..5 {
            f.sink
                .extracted("pub2", extracted(&format!("s{i}.txt"), "tiny"))
                .unwrap();
        }
        cfg_with(&f, &[("pub", "/data/pub")]);
        f.sink.reload(&f.cfg).unwrap();
        let (_, free) = pages(&f);
        assert!(free > 0);
        f.sink.reload(&f.cfg).unwrap();
        assert_eq!(pages(&f).1, free);
    }

    fn count(c: &Connection, sql: &str) -> i64 {
        c.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn files(s: &DbSink, t: Tier) -> i64 {
        count(s.conn(t), "SELECT count(*) FROM files")
    }

    fn entry(rel: &str) -> Entry {
        Entry {
            relpath: rel.into(),
            size: 5,
            mtime_ns: 7,
            is_dir: false,
        }
    }

    fn scan(complete: bool, entries: &[&str], skipped: &[(&str, &str)]) -> ScanResult {
        ScanResult {
            complete,
            entries: entries.iter().map(|e| entry(e)).collect(),
            skipped: skipped
                .iter()
                .map(|(a, b)| ((*a).into(), (*b).into()))
                .collect(),
        }
    }

    #[test]
    fn tier_routing_and_search_rows() {
        let mut f = fx("route");
        f.sink
            .extracted("pub", extracted("a.txt", "alpha public words"))
            .unwrap();
        f.sink
            .extracted("secret", extracted("b.txt", "beta private words"))
            .unwrap();
        assert_eq!(files(&f.sink, Tier::Public), 1);
        assert_eq!(files(&f.sink, Tier::Private), 1);
        assert_eq!(f.sink.known("pub", "a.txt"), Some((5, 7)));
        // FTS rows exist for the chunk and the file
        assert_eq!(
            count(
                f.sink.conn(Tier::Public),
                "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'alpha'"
            ),
            1
        );
        assert_eq!(
            count(
                f.sink.conn(Tier::Public),
                "SELECT count(*) FROM files_fts WHERE files_fts MATCH 'title'"
            ),
            1
        );
        // re-extract replaces, not duplicates
        f.sink
            .extracted("pub", extracted("a.txt", "alpha again"))
            .unwrap();
        assert_eq!(files(&f.sink, Tier::Public), 1);
        assert_eq!(
            count(f.sink.conn(Tier::Public), "SELECT count(*) FROM chunks"),
            1
        );
        assert_eq!(
            count(f.sink.conn(Tier::Public), "SELECT count(*) FROM chunks_fts"),
            1
        );
        // a denied file is never stored
        f.sink
            .extracted("pub", extracted(".env", "SECRET=1"))
            .unwrap();
        assert_eq!(files(&f.sink, Tier::Public), 1);
    }

    #[test]
    fn public_db_has_no_private_name_or_path() {
        let mut f = fx("leak");
        f.sink
            .extracted("secret", extracted("b.txt", "beta private words"))
            .unwrap();
        f.sink
            .extracted("pub", extracted("a.txt", "alpha"))
            .unwrap();
        f.sink.root_state("secret", "offline", "x");
        f.sink.set_expected_identity(
            "secret",
            &Share {
                fstype: "nfs".into(),
                source: "nas:/secret-things".into(),
                mount_root: "/".into(),
                mount_point: "/data/secret-things".into(),
            },
        );
        f.sink
            .validated("/data/secret-things", ValidationResult::Ok);
        f.sink
            .validated("/data/secret-things/link", ValidationResult::Eloop);
        let ck = |c: &Connection| c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        ck(&f.sink.store.public);
        let bytes = fs::read(f.dir.join("state/public.db")).unwrap();
        for needle in ["secret-things", "secret", "beta private"] {
            assert!(
                !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
                "public.db contains {needle}"
            );
        }
        let wal = fs::read(f.dir.join("state/public.db-wal")).unwrap_or_default();
        assert!(!wal.windows(6).any(|w| w == b"secret"));
        assert_eq!(
            count(
                f.sink.conn(Tier::Public),
                "SELECT count(*) FROM roots WHERE name = 'secret'"
            ),
            0
        );
    }

    #[test]
    fn validation_hashes_in_both_dbs_clear_path_only_in_private() {
        let mut f = fx("valid");
        f.sink
            .validated("/data/secret-things", ValidationResult::Ok);
        f.sink
            .validated("/data/secret-things/link", ValidationResult::Eloop);
        f.sink
            .validated("/data/secret-things/later", ValidationResult::Pending);
        for t in TIERS {
            assert_eq!(count(f.sink.conn(t), "SELECT count(*) FROM validated"), 2);
            assert_eq!(
                count(
                    f.sink.conn(t),
                    "SELECT count(*) FROM validated WHERE config_error = 1"
                ),
                1
            );
        }
        assert_eq!(
            count(
                f.sink.conn(Tier::Private),
                "SELECT count(*) FROM config_errors WHERE path = '/data/secret-things/link'"
            ),
            1
        );
        assert!(
            f.sink
                .conn(Tier::Public)
                .prepare("SELECT 1 FROM config_errors")
                .is_err(),
            "public.db has no config_errors table"
        );
        f.sink
            .validated("/data/secret-things/link", ValidationResult::Ok);
        assert_eq!(
            count(
                f.sink.conn(Tier::Private),
                "SELECT count(*) FROM config_errors"
            ),
            0
        );
    }

    #[test]
    fn unsafe_name_row_is_deleted_by_a_complete_scan() {
        let mut f = fx("unsafe-row");
        let bad = "d\u{202e}x/a.txt";
        f.sink.extracted("pub", extracted("keep.txt", "k")).unwrap();
        f.sink.extracted("pub", extracted(bad, "b")).unwrap();
        assert_eq!(files(&f.sink, Tier::Public), 2);
        f.sink.scan_done(
            "pub",
            scan(true, &["keep.txt"], &[("d\u{202e}x", "unsafe_name")]),
        );
        assert_eq!(files(&f.sink, Tier::Public), 1);
    }

    #[test]
    fn deletion_needs_a_complete_scan() {
        let mut f = fx("del");
        f.sink.extracted("pub", extracted("keep.txt", "k")).unwrap();
        f.sink.extracted("pub", extracted("gone.txt", "g")).unwrap();
        f.sink.scan_done("pub", scan(false, &["keep.txt"], &[]));
        assert_eq!(
            files(&f.sink, Tier::Public),
            2,
            "incomplete scan deletes nothing"
        );
        f.sink.scan_done("pub", scan(true, &["keep.txt"], &[]));
        assert_eq!(files(&f.sink, Tier::Public), 1);
        assert_eq!(
            count(f.sink.conn(Tier::Public), "SELECT count(*) FROM chunks"),
            1
        );
        assert_eq!(
            count(f.sink.conn(Tier::Public), "SELECT count(*) FROM files_fts"),
            1
        );
        let (q, d) = f
            .sink
            .conn(Tier::Public)
            .query_row(
                "SELECT queued, done FROM roots WHERE name = 'pub'",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .unwrap();
        assert_eq!((q, d), (0, 1));
    }

    #[test]
    fn mount_and_errno_skips_protect_rows_rule_skips_do_not() {
        let mut f = fx("skips");
        for rel in [
            "mnt/a.txt",
            "locked/b.txt",
            "ign/c.txt",
            "io/d.txt",
            "plain.txt",
        ] {
            f.sink.extracted("pub", extracted(rel, "x")).unwrap();
        }
        f.sink.scan_done(
            "pub",
            scan(
                true,
                &[],
                &[
                    ("mnt", "mount"),
                    ("locked", "EACCES"),
                    ("ign", "ignored"),
                    ("io", "EIO"),
                ],
            ),
        );
        let left: Vec<String> = f
            .sink
            .conn(Tier::Public)
            .prepare("SELECT relpath FROM files ORDER BY relpath")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(left, ["io/d.txt", "locked/b.txt", "mnt/a.txt"]);
    }

    #[test]
    fn expected_identity_round_trips_and_state_dir_inside_root_is_denied() {
        let mut f = fx("ident");
        let s = Share {
            fstype: "nfs".into(),
            source: "nas:/x".into(),
            mount_root: "/".into(),
            mount_point: "/data/pub".into(),
        };
        assert_eq!(f.sink.expected_identity("pub"), None);
        f.sink.set_expected_identity("pub", &s);
        assert_eq!(f.sink.expected_identity("pub"), Some(s));
        assert_eq!(f.sink.indexed_files("pub"), 0);
    }

    #[test]
    fn indexed_files_is_zero_or_one_and_stops_at_the_first_database() {
        use rusqlite::functions::FunctionFlags;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut f = fx("indexed-exists");
        assert_eq!(f.sink.indexed_files("pub"), 0);
        f.sink.extracted("pub", extracted("a.txt", "a")).unwrap();
        f.sink.extracted("pub", extracted("b.txt", "b")).unwrap();
        assert_eq!(f.sink.indexed_files("pub"), 1); // a count, not 2
        // Querying this view calls observe(); the first database must short-circuit it.
        let queries = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&queries);
        f.sink
            .conn(Tier::Private)
            .create_scalar_function("observe", 0, FunctionFlags::SQLITE_UTF8, move |_| {
                observed.fetch_add(1, Ordering::Relaxed);
                Ok("pub")
            })
            .unwrap();
        f.sink
            .conn(Tier::Private)
            .execute_batch(
                "DROP TABLE files; CREATE VIEW files AS SELECT observe() AS root_id, 'ok' AS state",
            )
            .unwrap();
        assert_eq!(f.sink.indexed_files("pub"), 1);
        assert_eq!(queries.load(Ordering::Relaxed), 0);
        assert_eq!(f.sink.indexed_files("other"), 0);
        assert!(queries.load(Ordering::Relaxed) > 0);
        // an error answers "some", so adoption stays conservative
        f.sink
            .conn(Tier::Private)
            .execute_batch("DROP VIEW files")
            .unwrap();
        assert_eq!(f.sink.indexed_files("other"), 1);
    }

    fn move_fixture(name: &str) -> Fx {
        let mut f = fx(name);
        f.sink
            .extracted("moving", extracted("m.txt", "movable words"))
            .unwrap();
        assert_eq!(files(&f.sink, Tier::Public), 1);
        // config now makes the nested root private
        fs::write(
            &f.cfg,
            format!(
                "state_dir = \"{}\"\n[[root]]\nname = \"pub\"\npath = \"/data/pub\"\ntier = \"public\"\n\
                 [[root]]\nname = \"moving\"\npath = \"/data/pub/moving\"\ntier = \"private\"\n",
                f.dir.join("state").display()
            ),
        )
        .unwrap();
        f
    }

    #[test]
    fn public_to_private_move_leaves_nothing_in_public_even_after_a_crash() {
        let mut f = move_fixture("crash");
        f.sink.abort_after_first_commit = true;
        assert!(f.sink.reload(&f.cfg).is_err()); // died between the two transactions
        assert_eq!(
            files(&f.sink, Tier::Public),
            0,
            "public delete was committed first"
        );
        assert_eq!(
            count(f.sink.conn(Tier::Public), "SELECT count(*) FROM chunks"),
            0
        );
        assert_eq!(
            count(f.sink.conn(Tier::Public), "SELECT count(*) FROM chunks_fts"),
            0
        );
        assert_eq!(files(&f.sink, Tier::Private), 0);
    }

    #[test]
    fn public_to_private_move_completes() {
        let mut f = move_fixture("move");
        f.sink.reload(&f.cfg).unwrap();
        assert_eq!(files(&f.sink, Tier::Public), 0);
        assert_eq!(files(&f.sink, Tier::Private), 1);
        assert_eq!(
            count(
                f.sink.conn(Tier::Private),
                "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'movable'"
            ),
            1
        );
        assert_eq!(
            count(
                f.sink.conn(Tier::Public),
                "SELECT count(*) FROM roots WHERE name = 'moving'"
            ),
            0
        );
        assert_eq!(
            count(
                f.sink.conn(Tier::Private),
                "SELECT count(*) FROM roots WHERE name = 'moving'"
            ),
            1
        );
    }

    #[test]
    fn purge_removes_rows_a_new_exclusion_rejects() {
        let mut f = fx("purge");
        f.sink
            .extracted("pub", extracted("docs/a.txt", "a"))
            .unwrap();
        f.sink
            .extracted("pub", extracted("keep/b.txt", "b"))
            .unwrap();
        fs::write(
            &f.cfg,
            format!(
                "state_dir = \"{}\"\n[[root]]\nname = \"pub\"\npath = \"/data/pub\"\ntier = \"public\"\n\
                 [exclude]\npaths = [\"/data/pub/docs\"]\n",
                f.dir.join("state").display()
            ),
        )
        .unwrap();
        f.sink.reload(&f.cfg).unwrap();
        assert_eq!(files(&f.sink, Tier::Public), 1);
        assert_eq!(
            count(f.sink.conn(Tier::Public), "SELECT count(*) FROM chunks_fts"),
            1
        );
    }

    #[test]
    fn prune_drops_legacy_rows_with_an_unsafe_path() {
        let mut f = fx("prune-unsafe");
        f.sink
            .extracted("pub", extracted("a\u{202e}b.txt", "bad"))
            .unwrap();
        f.sink
            .extracted("pub", extracted("ok.txt", "good"))
            .unwrap();
        assert_eq!(files(&f.sink, Tier::Public), 2);
        f.sink.reload(&f.cfg).unwrap();
        assert_eq!(files(&f.sink, Tier::Public), 1);
        assert_eq!(
            count(f.sink.conn(Tier::Public), "SELECT count(*) FROM chunks"),
            1
        );
    }

    fn share_of(f: &Fx, t: Tier, root: &str) -> Option<String> {
        f.sink
            .conn(t)
            .query_row("SELECT share_id FROM roots WHERE name = ?1", [root], |r| {
                r.get(0)
            })
            .unwrap()
    }

    #[test]
    fn share_id_follows_a_tier_move_and_clears_on_a_path_change() {
        let mut f = move_fixture("shareid");
        let s = Share {
            fstype: "nfs".into(),
            source: "nas:/m".into(),
            mount_root: "/".into(),
            mount_point: "/data/pub/moving".into(),
        };
        f.sink.set_expected_identity("moving", &s);
        f.sink.reload(&f.cfg).unwrap();
        assert_eq!(
            share_of(&f, Tier::Private, "moving"),
            Some(serde_json::to_string(&s).unwrap())
        );
        assert_eq!(f.sink.expected_identity("moving"), Some(s));
        // same tier, other path: the share is derived again
        fs::write(
            &f.cfg,
            format!(
                "state_dir = \"{}\"\n[[root]]\nname = \"pub\"\npath = \"/data/pub\"\ntier = \"public\"\n\
                 [[root]]\nname = \"moving\"\npath = \"/data/pub/elsewhere\"\ntier = \"private\"\n",
                f.dir.join("state").display()
            ),
        )
        .unwrap();
        f.sink.reload(&f.cfg).unwrap();
        assert_eq!(share_of(&f, Tier::Private, "moving"), None);
    }

    #[test]
    fn rows_and_validation_of_removed_roots_are_purged() {
        let mut f = fx("orphans");
        f.sink
            .extracted("pub", extracted("a.txt", "alpha"))
            .unwrap();
        f.sink
            .extracted("secret", extracted("b.txt", "beta"))
            .unwrap();
        f.sink
            .validated("/data/secret-things", ValidationResult::Eloop);
        f.sink.validated("/data/pub", ValidationResult::Ok);
        fs::write(
            &f.cfg,
            format!(
                "state_dir = \"{}\"\n[[root]]\nname = \"pub\"\npath = \"/data/pub\"\ntier = \"public\"\n",
                f.dir.join("state").display()
            ),
        )
        .unwrap();
        f.sink.reload(&f.cfg).unwrap();
        assert_eq!(files(&f.sink, Tier::Public), 1);
        assert_eq!(files(&f.sink, Tier::Private), 0);
        assert_eq!(
            count(f.sink.conn(Tier::Private), "SELECT count(*) FROM chunks"),
            0
        );
        for t in TIERS {
            assert_eq!(count(f.sink.conn(t), "SELECT count(*) FROM validated"), 1);
        }
        assert_eq!(
            count(
                f.sink.conn(Tier::Private),
                "SELECT count(*) FROM config_errors"
            ),
            0
        );
    }

    #[test]
    fn model_revision_change_resets_vectors_and_sizes_not_mtime() {
        let mut f = fx("modelrev");
        f.sink
            .extracted("pub", extracted("a.txt", "alpha"))
            .unwrap();
        for t in TIERS {
            f.sink
                .conn(t)
                .execute("UPDATE meta SET model_rev = 'old'", [])
                .unwrap();
        }
        f.sink
            .conn(Tier::Public)
            .execute("UPDATE chunks SET vec = zeroblob(512)", [])
            .unwrap();
        let dir = f.dir.join("state");
        f.sink = DbSink::open_with(&dir, &f.cfg, None).unwrap();
        let c = f.sink.conn(Tier::Public);
        assert_eq!(
            count(c, "SELECT count(*) FROM chunks WHERE vec IS NOT NULL"),
            0
        );
        assert_eq!(count(c, "SELECT size FROM files"), -1);
        assert_eq!(count(c, "SELECT mtime_ns FROM files"), 7);
        let rev: String = c
            .query_row("SELECT model_rev FROM meta", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rev, embed::MODEL_REV);
    }

    #[test]
    fn embed_cache_table_is_gone_after_open() {
        let mut f = fx("nocache");
        let c = f.sink.conn(Tier::Public);
        c.execute(
            "CREATE TABLE embed_cache(input_blake3 BLOB PRIMARY KEY, vector BLOB)",
            [],
        )
        .unwrap();
        let dir = f.dir.join("state");
        f.sink = DbSink::open_with(&dir, &f.cfg, None).unwrap();
        for t in TIERS {
            let q = "SELECT count(*) FROM sqlite_master WHERE name = 'embed_cache'";
            assert_eq!(count(f.sink.conn(t), q), 0);
        }
    }

    #[test]
    fn unchanged_chunks_keep_their_vector_and_are_not_embedded() {
        let mut f = fx("reuse");
        let two = |second: &str| {
            let mut ef = extracted("a.txt", "keep me");
            ef.chunks.push(Chunk {
                ord: 1,
                text: second.into(),
                ..ef.chunks[0].clone()
            });
            ef
        };
        f.sink.extracted("pub", two("second")).unwrap();
        f.sink
            .conn(Tier::Public)
            .execute("UPDATE chunks SET vec = zeroblob(512)", [])
            .unwrap();
        f.sink.embed_calls.set(0);
        f.sink.extracted("pub", two("changed")).unwrap();
        assert_eq!(
            f.sink.embed_calls.get(),
            1,
            "only the changed chunk is embedded"
        );
        let c = f.sink.conn(Tier::Public);
        assert_eq!(
            count(
                c,
                "SELECT count(*) FROM chunks WHERE text = 'keep me' AND length(vec) = 512"
            ),
            1
        );
    }

    fn many(name: &str, n: usize) -> Fx {
        let mut f = fx(name);
        for i in 0..n {
            f.sink
                .extracted("moving", extracted(&format!("m{i}.txt"), "w"))
                .unwrap();
        }
        assert_eq!(files(&f.sink, Tier::Public), n as i64);
        fs::write(
            &f.cfg,
            format!(
                "state_dir = \"{}\"\n[[root]]\nname = \"pub\"\npath = \"/data/pub\"\ntier = \"public\"\n\
                 [[root]]\nname = \"moving\"\npath = \"/data/pub/moving\"\ntier = \"private\"\n",
                f.dir.join("state").display()
            ),
        )
        .unwrap();
        f.sink.embed_calls.set(0);
        f
    }

    #[test]
    fn tier_move_works_in_batches_of_256() {
        let mut f = many("batches", 1000);
        f.sink.reload(&f.cfg).unwrap();
        assert_eq!(*f.sink.move_batches.borrow(), vec![256, 256, 256, 232]);
        assert_eq!(files(&f.sink, Tier::Public), 0);
        assert_eq!(files(&f.sink, Tier::Private), 1000);
    }

    #[test]
    fn tier_move_crash_after_first_batch_keeps_the_rest_in_the_source() {
        let mut f = many("batchcrash", 1000);
        f.sink.abort_after_first_commit = true;
        assert!(f.sink.reload(&f.cfg).is_err());
        assert_eq!(files(&f.sink, Tier::Public), 744);
        assert_eq!(files(&f.sink, Tier::Private), 0);
    }
}
