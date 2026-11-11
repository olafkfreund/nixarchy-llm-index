//! SQLite storage: `public.db` and `private.db` with one schema, plus the `dot_i8` and
//! `visible` scalar functions. A public-only store never opens or can attach `private.db`.

use std::error::Error;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::functions::FunctionFlags;
use rusqlite::limits::Limit;
use rusqlite::{Connection, OpenFlags};

use crate::auth::{self, RootState, RootStates, Validated};
use crate::config::{Config, Tier};

type Res<T> = Result<T, Box<dyn Error>>;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS roots(name TEXT PRIMARY KEY, path TEXT, kind TEXT, share_id TEXT,
  state TEXT, reason TEXT, last_scan INTEGER, queued INTEGER, done INTEGER, skipped_symlinks INTEGER);
CREATE TABLE IF NOT EXISTS files(id INTEGER PRIMARY KEY, root_id TEXT NOT NULL, relpath TEXT NOT NULL,
  abspath TEXT NOT NULL, title TEXT, type TEXT, size INTEGER, mtime_ns INTEGER, blake3 BLOB,
  state TEXT, reason TEXT, UNIQUE(root_id, relpath));
CREATE TABLE IF NOT EXISTS chunks(id INTEGER PRIMARY KEY, file_id INTEGER NOT NULL, ord INTEGER,
  kind TEXT, start INTEGER, end INTEGER, decl_start INTEGER, decl_end INTEGER,
  heading_or_symbol TEXT, text TEXT, vec BLOB);
CREATE INDEX IF NOT EXISTS chunks_file ON chunks(file_id, ord);
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(text, tokenize='unicode61 remove_diacritics 2');
CREATE VIRTUAL TABLE IF NOT EXISTS files_fts USING fts5(title, path, tokenize='unicode61 remove_diacritics 2');
CREATE TABLE IF NOT EXISTS embed_cache(input_blake3 BLOB PRIMARY KEY, vector BLOB);
CREATE TABLE IF NOT EXISTS validated(path_blake3 BLOB PRIMARY KEY, config_error INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS meta(model_rev TEXT, schema_version INTEGER);
";
const PRIVATE_ONLY: &str =
    "CREATE TABLE IF NOT EXISTS config_errors(path TEXT PRIMARY KEY, reason TEXT);";

/// `Writer` (the indexer) creates files 0600, sets WAL and the schema. Readers open
/// read-only with `query_only` and never run DDL.
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Mode {
    PublicOnly,
    Both,
    Writer,
}

fn open(path: &Path, mode: Mode, private: bool) -> Res<Connection> {
    let conn = if mode == Mode::Writer {
        // 0600 on creation; the state dir itself is 0700.
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        let conn = Connection::open_with_flags(path, OpenFlags::default())?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        if private {
            conn.execute_batch(PRIVATE_ONLY)?;
        }
        conn
    } else {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags)?;
        conn.pragma_update(None, "query_only", 1)?;
        conn
    };
    conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?; // no ATTACH, ever
    conn.create_scalar_function(
        "dot_i8",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let (a, b) = (ctx.get_raw(0), ctx.get_raw(1));
            let (Ok(a), Ok(b)) = (a.as_blob(), b.as_blob()) else {
                return Ok(None);
            };
            if a.len() != crate::embed::DIM || b.len() != crate::embed::DIM {
                return Err(rusqlite::Error::UserFunctionError(
                    "dot_i8 needs two 512-byte blobs".into(),
                ));
            }
            let sum: i32 = a
                .iter()
                .zip(b)
                .map(|(&x, &y)| (x as i8 as i32) * (y as i8 as i32))
                .sum();
            Ok(Some(sum as i64))
        },
    )?;
    Ok(conn)
}

#[derive(Default)]
struct State {
    config: Option<Config>,
    states: RootStates,
    validated: Validated,
}

pub struct Store {
    pub mode: Mode,
    pub public: Connection,
    pub private: Option<Connection>,
    state: Arc<Mutex<State>>,
}

/// One read transaction per connection; ends when dropped.
pub struct Snapshot<'a>(&'a Store);

impl Drop for Snapshot<'_> {
    fn drop(&mut self) {
        for conn in self.0.conns() {
            let _ = conn.execute_batch("COMMIT");
        }
    }
}

impl Store {
    /// `PublicOnly` opens `public.db` only; nothing here can reach `private.db`.
    pub fn open(dir: &Path, mode: Mode) -> Res<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let open_db = |name: &str, private| -> Res<Connection> {
            let conn = open(&dir.join(name), mode, private)?;
            let st = state.clone();
            // Without a config, nothing is visible.
            conn.create_scalar_function("visible", 1, FunctionFlags::SQLITE_UTF8, move |ctx| {
                let path = ctx.get::<String>(0)?;
                let guard = st.lock().unwrap();
                let tier = guard
                    .config
                    .as_ref()
                    .and_then(|c| auth::visible(&path, c, &guard.states, &guard.validated));
                Ok(tier.map(|t| {
                    if t == Tier::Public {
                        "public"
                    } else {
                        "private"
                    }
                }))
            })?;
            Ok(conn)
        };
        let public = open_db("public.db", false)?;
        let private = if mode == Mode::PublicOnly {
            None
        } else {
            Some(open_db("private.db", true)?)
        };
        Ok(Self {
            mode,
            public,
            private,
            state,
        })
    }

    fn conns(&self) -> impl Iterator<Item = &Connection> {
        std::iter::once(&self.public).chain(self.private.as_ref())
    }

    /// The caller's in-memory config; root states and `validated` come from the DBs per call.
    pub fn set_config(&self, config: Config) {
        self.state.lock().unwrap().config = Some(config);
    }

    /// Start of every tool call: open a read transaction on each DB, then load `roots`
    /// states and `validated` inside it, so every leg sees one snapshot. No caching.
    pub fn begin(&self) -> Res<Snapshot<'_>> {
        let snap = Snapshot(self);
        for conn in self.conns() {
            conn.execute_batch("BEGIN DEFERRED")?;
        }
        let (mut states, mut validated) = (RootStates::new(), Validated::default());
        for conn in self.conns() {
            let mut stmt = conn.prepare("SELECT name, state FROM roots")?;
            for row in stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })? {
                let (name, st) = row?;
                if st.as_deref() == Some("config_error") {
                    states.insert(name, RootState::ConfigError);
                }
            }
            let mut stmt = conn.prepare("SELECT path_blake3, config_error FROM validated")?;
            for row in stmt.query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)))? {
                let (hash, err) = row?;
                let hash =
                    blake3::Hash::from_bytes(hash.try_into().map_err(|_| "bad validated hash")?);
                if err == 0 {
                    validated.ok.insert(hash)
                } else {
                    validated.errors.insert(hash)
                };
            }
        }
        let mut st = self.state.lock().unwrap();
        st.states = states;
        st.validated = validated;
        Ok(snap)
    }

    /// Public server: once per tool call, after `begin`. The private server needs none.
    pub fn gate(&self) -> Result<(), &'static str> {
        if self.mode != Mode::PublicOnly {
            return Ok(());
        }
        let st = self.state.lock().unwrap();
        match st.config.as_ref() {
            Some(c) => auth::public_gate(c, &st.states, &st.validated),
            None => Err(auth::PUBLIC_CHECKING),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("llm-index-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn public_only_cannot_reach_private() {
        let dir = tmp("attach");
        drop(Store::open(&dir, Mode::Writer).unwrap()); // both files now exist
        let s = Store::open(&dir, Mode::PublicOnly).unwrap();
        assert!(s.private.is_none());
        let attach = format!("ATTACH '{}' AS p", dir.join("private.db").display());
        assert!(s.public.execute_batch(&attach).is_err());
        let tables: i64 = s
            .public
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='config_errors'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0);
    }

    #[test]
    fn readers_are_read_only_and_writer_makes_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("ro");
        assert!(Store::open(&dir, Mode::PublicOnly).is_err()); // a reader never creates files
        drop(Store::open(&dir, Mode::Writer).unwrap());
        let mode = std::fs::metadata(dir.join("public.db"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        for m in [Mode::PublicOnly, Mode::Both] {
            let s = Store::open(&dir, m).unwrap();
            for c in s.conns() {
                assert!(c.execute_batch("INSERT INTO meta VALUES('x', 1)").is_err());
                assert!(c.execute_batch("CREATE TABLE t(a)").is_err());
            }
        }
    }

    #[test]
    fn dot_product() {
        let dir = tmp("dot");
        drop(Store::open(&dir, Mode::Writer).unwrap());
        let s = Store::open(&dir, Mode::PublicOnly).unwrap();
        let (a, b) = (vec![2u8; 512], vec![(-3i8) as u8; 512]);
        let d: i64 = s
            .public
            .query_row("SELECT dot_i8(?, ?)", [&a, &b], |r| r.get(0))
            .unwrap();
        assert_eq!(d, -3072);
    }
}
