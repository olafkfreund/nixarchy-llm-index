//! Eligibility, the words and meaning legs, and reciprocal-rank fusion.

use std::collections::HashMap;
use std::error::Error;
use std::hash::Hash;

use rusqlite::Connection;

use crate::db::{Mode as StoreMode, Store};
use crate::embed::DIM;

type Res<T> = Result<T, Box<dyn Error>>;
const RRF_K: f64 = 60.0;

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum SearchMode {
    Hybrid,
    Words,
    Meaning,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Field {
    Any,
    Title,
    Path,
}

pub struct Query<'a> {
    pub text: &'a str,
    pub mode: SearchMode,
    pub field: Field,
    pub folder: Option<&'a str>,
    pub file_type: Option<&'a str>,
    pub after_ns: Option<i64>,
    pub before_ns: Option<i64>,
    pub limit: usize,
}

/// `src` 0 = public.db, 1 = private.db.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Hit {
    pub src: u8,
    pub file_id: i64,
    pub chunk_id: Option<i64>,
    pub abspath: String,
}

/// Reciprocal-rank fusion; ties keep first appearance.
pub fn rrf<K: Hash + Eq + Clone>(lists: &[Vec<K>]) -> Vec<K> {
    let mut score: HashMap<&K, (f64, usize)> = HashMap::new();
    for list in lists {
        for (rank, k) in list.iter().enumerate() {
            let n = score.len();
            score.entry(k).or_insert((0.0, n)).0 += 1.0 / (RRF_K + rank as f64 + 1.0);
        }
    }
    let mut out: Vec<_> = score.into_iter().collect();
    out.sort_by(|a, b| b.1.0.total_cmp(&a.1.0).then(a.1.1.cmp(&b.1.1)));
    out.into_iter().map(|(k, _)| k.clone()).collect()
}

/// Quoted phrases stay phrases, other words become quoted tokens; OR-joined so BM25 ranks.
/// Control characters are stripped; an unbalanced quote makes the rest plain words.
fn fts_expr(text: &str) -> Option<String> {
    let clean: String = text
        .chars()
        .filter(|c| !c.is_control() || c.is_whitespace())
        .collect();
    let parts: Vec<_> = clean.split('"').collect();
    let mut terms = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if i % 2 == 1 && i + 1 < parts.len() {
            terms.push(part.trim().to_string());
        } else {
            terms.extend(part.split_whitespace().map(str::to_string));
        }
    }
    let terms: Vec<_> = terms
        .into_iter()
        .filter(|t| t.chars().any(char::is_alphanumeric))
        .collect();
    (!terms.is_empty()).then(|| {
        terms
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(" OR ")
    })
}

/// The tier test every query on `f.abspath` carries.
pub fn visible_sql(mode: StoreMode) -> &'static str {
    if mode == StoreMode::PublicOnly {
        "visible(f.abspath) = 'public'"
    } else {
        "visible(f.abspath) IS NOT NULL"
    }
}

// Cheap column filters first; `visible` last. Public server rows must be public.
// `:folder` matches case-sensitively by whole path components.
pub fn eligible(mode: StoreMode) -> String {
    let vis = visible_sql(mode);
    format!(
        "(:folder IS NULL OR f.abspath = :folder
           OR substr(f.abspath, 1, length(:folder) + 1) = :folder || '/')
         AND (:ty IS NULL OR f.type = :ty) AND (:after IS NULL OR f.mtime_ns >= :after)
         AND (:before IS NULL OR f.mtime_ns <= :before) AND {vis}"
    )
}

const FIRST_CHUNK: &str = "(SELECT id FROM chunks WHERE file_id = f.id ORDER BY ord LIMIT 1)";

fn run(
    conn: &Connection,
    sql: &str,
    q: &Query,
    src: u8,
    extra: &[(&str, &dyn rusqlite::ToSql)],
    pool: usize,
) -> Res<Vec<(Hit, i64)>> {
    let folder = norm_folder(q.folder)?;
    let pool = pool as i64;
    let mut params: Vec<(&str, &dyn rusqlite::ToSql)> = vec![
        (":folder", &folder),
        (":ty", &q.file_type),
        (":after", &q.after_ns),
        (":before", &q.before_ns),
        (":pool", &pool),
    ];
    params.extend_from_slice(extra);
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params.as_slice(), |r| {
        let hit = Hit {
            src,
            file_id: r.get(0)?,
            chunk_id: r.get(1)?,
            abspath: r.get(2)?,
        };
        Ok((hit, r.get::<_, Option<i64>>(3)?.unwrap_or(0)))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn meaning_leg(
    store: &Store,
    conn: &Connection,
    src: u8,
    q: &Query,
    v: &[u8],
    pool: usize,
) -> Res<Vec<(Hit, i64)>> {
    let sql = format!(
        "SELECT f.id, c.id, f.abspath, dot_i8(c.vec, :v) AS s FROM chunks c JOIN files f ON f.id = c.file_id
         WHERE c.vec IS NOT NULL AND {} ORDER BY s DESC LIMIT :pool",
        eligible(store.mode)
    );
    run(conn, &sql, q, src, &[(":v", &v)], pool)
}

/// One fused words list for one DB: `chunks_fts` and `files_fts` hits deduplicated by chunk
/// (a title/path hit maps to the file's first chunk). With `field` title/path only
/// `files_fts` runs.
fn words_list(store: &Store, conn: &Connection, src: u8, q: &Query, pool: usize) -> Res<Vec<Hit>> {
    let Some(expr) = fts_expr(q.text) else {
        return Ok(vec![]);
    };
    let elig = eligible(store.mode);
    let mut lists = Vec::new();
    if q.field == Field::Any {
        let sql = format!(
            "SELECT f.id, c.id, f.abspath, 0 FROM chunks_fts JOIN chunks c ON c.id = chunks_fts.rowid
             JOIN files f ON f.id = c.file_id WHERE chunks_fts MATCH :m AND {elig}
             ORDER BY bm25(chunks_fts) LIMIT :pool"
        );
        lists.push(run(conn, &sql, q, src, &[(":m", &expr)], pool)?);
    }
    let col = match q.field {
        Field::Any => "",
        Field::Title => "title : ",
        Field::Path => "path : ",
    };
    let m = format!("{col}({expr})");
    let sql = format!(
        "SELECT f.id, {FIRST_CHUNK}, f.abspath, 0 FROM files_fts JOIN files f ON f.id = files_fts.rowid
         WHERE files_fts MATCH :m AND {elig} ORDER BY bm25(files_fts) LIMIT :pool"
    );
    lists.push(run(conn, &sql, q, src, &[(":m", &m)], pool)?);
    let lists: Vec<Vec<Hit>> = lists
        .into_iter()
        .map(|l| l.into_iter().map(|(h, _)| h).collect())
        .collect();
    let mut fused = rrf(&lists);
    fused.truncate(pool);
    Ok(fused)
}

/// `qvec` feeds the meaning leg; `None` (no usable query token) skips it, leaving words only.
/// `mode=Meaning` with `field` title/path cannot use vectors, so it runs words over
/// `files_fts` only. `mode=Meaning` with `field=Any` runs the meaning leg alone.
/// One snapshot per call; the public store re-checks its gate here.
pub fn search(store: &Store, q: &Query, qvec: Option<&[i8; DIM]>) -> Res<Vec<Hit>> {
    let _snapshot = store.begin()?;
    search_in_snapshot(store, q, qvec)
}

/// The folder argument as the SQL expects it: absolute, no trailing `/`.
pub fn norm_folder(folder: Option<&str>) -> Res<Option<&str>> {
    match folder {
        Some(f) if !f.starts_with('/') => Err("folder must be an absolute path".into()),
        f => Ok(f.map(|f| f.trim_end_matches('/'))),
    }
}

/// `search` inside a snapshot the caller holds (`Store::begin`), so the caller can read
/// hit details from the same snapshot.
pub fn search_in_snapshot(store: &Store, q: &Query, qvec: Option<&[i8; DIM]>) -> Res<Vec<Hit>> {
    store.gate()?;
    norm_folder(q.folder)?;
    let limit = q.limit.clamp(1, 50);
    let pool = limit.saturating_mul(4);
    let conns: Vec<(u8, &Connection)> = std::iter::once((0, &store.public))
        .chain(store.private.as_ref().map(|c| (1, c)))
        .collect();
    let mut lists: Vec<Vec<Hit>> = Vec::new();
    if q.mode != SearchMode::Meaning || q.field != Field::Any {
        for &(src, conn) in &conns {
            lists.push(words_list(store, conn, src, q, pool)?);
        }
    }
    if let (true, Some(qvec)) = (q.mode != SearchMode::Words && q.field == Field::Any, qvec) {
        let v: Vec<u8> = qvec.iter().map(|&x| x as u8).collect();
        let mut all = Vec::new();
        for &(src, conn) in &conns {
            all.extend(meaning_leg(store, conn, src, q, &v, pool)?);
        }
        all.sort_by_key(|a| std::cmp::Reverse(a.1)); // stable: public first on ties
        all.truncate(pool);
        lists.push(all.into_iter().map(|(h, _)| h).collect());
    }
    let by_key: HashMap<&Hit, &Hit> = lists.iter().flatten().map(|h| (h, h)).collect();
    let mut hits: Vec<Hit> = rrf(&lists)
        .into_iter()
        .map(|h| by_key[&h].clone())
        .collect();
    hits.truncate(limit);
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::path_hash;
    use crate::config::Config;
    use crate::db::tests::tmp;
    use std::path::{Path, PathBuf};

    fn vec_of(hot: usize) -> Vec<u8> {
        let mut v = vec![0u8; DIM];
        v[hot] = 100;
        v[(hot + 1) % DIM] = 20;
        v
    }

    struct Fx {
        dir: PathBuf,
        w: Store,
    }

    impl Fx {
        fn new(name: &str) -> Fx {
            let dir = tmp(name);
            let w = Store::open(&dir, StoreMode::Writer).unwrap();
            for p in ["/home/x", "/home/x/priv"] {
                let h = path_hash(Path::new(p)).unwrap();
                w.public
                    .execute(
                        "INSERT INTO validated VALUES(?, 0)",
                        [h.as_bytes().as_slice()],
                    )
                    .unwrap();
            }
            Fx { dir, w }
        }

        fn store(&self, mode: StoreMode) -> Store {
            let s = Store::open(&self.dir, mode).unwrap();
            let cfg: Config = toml::from_str(
                "state_dir = \"/state\"\n[[root]]\nname=\"h\"\npath=\"/home/x\"\ntier=\"public\"\n\
                 [[root]]\nname=\"p\"\npath=\"/home/x/priv\"\ntier=\"private\"\n",
            )
            .unwrap();
            s.set_config(cfg);
            s
        }

        fn search(&self, mode: StoreMode, q: &Query, v: Option<&[i8; DIM]>) -> Res<Vec<Hit>> {
            search(&self.store(mode), q, v)
        }
    }

    fn add_titled(conn: &Connection, abspath: &str, title: &str, text: &str, hot: usize) {
        conn.execute(
            "INSERT INTO files(root_id, relpath, abspath, title, type, mtime_ns) VALUES('h', ?1, ?1, ?2, 'text', 1)",
            [abspath, title],
        )
        .unwrap();
        let fid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO chunks(file_id, ord, text, vec) VALUES(?1, 0, ?2, ?3)",
            rusqlite::params![fid, text, vec_of(hot)],
        )
        .unwrap();
        let cid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO chunks_fts(rowid, text) VALUES(?1, ?2)",
            rusqlite::params![cid, text],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO files_fts(rowid, title, path) VALUES(?1, ?2, ?3)",
            rusqlite::params![fid, title, abspath],
        )
        .unwrap();
    }

    fn add(conn: &Connection, abspath: &str, text: &str, hot: usize) {
        add_titled(conn, abspath, abspath, text, hot);
    }

    fn q<'a>(text: &'a str, mode: SearchMode) -> Query<'a> {
        Query {
            text,
            mode,
            field: Field::Any,
            folder: None,
            file_type: None,
            after_ns: None,
            before_ns: None,
            limit: 1,
        }
    }

    fn qv(hot: usize) -> [i8; DIM] {
        vec_of(hot)
            .iter()
            .map(|&x| x as i8)
            .collect::<Vec<_>>()
            .try_into()
            .unwrap()
    }

    const PUB: StoreMode = StoreMode::PublicOnly;

    #[test]
    fn nearest_first_and_filter_before_limit() {
        let fx = Fx::new("meaning");
        // Query qv(5): hot-5 rows score 10400, hot-4 scores 2000, hot-200 scores 0.
        // The 20 ineligible rows are the globally nearest; filtering after LIMIT finds nothing.
        for i in 0..20 {
            add(&fx.w.public, &format!("/home/x/near/{i}.txt"), "zzz", 5);
        }
        add(&fx.w.public, "/home/x/far/a.txt", "zzz", 4);
        add(&fx.w.public, "/home/x/far/b.txt", "zzz", 200);
        let mut query = q("", SearchMode::Meaning);
        let unfiltered = fx.search(PUB, &query, Some(&qv(5))).unwrap();
        assert!(unfiltered[0].abspath.starts_with("/home/x/near/"));
        query.folder = Some("/home/x/far");
        let hits = fx.search(PUB, &query, Some(&qv(5))).unwrap();
        assert_eq!(hits[0].abspath, "/home/x/far/a.txt");
        query.folder = Some("/home/x/fa"); // component-wise prefix
        assert!(fx.search(PUB, &query, Some(&qv(5))).unwrap().is_empty());
        query.folder = Some("/home/x/FAR"); // case-sensitive
        assert!(fx.search(PUB, &query, Some(&qv(5))).unwrap().is_empty());
        for bad in ["", "far"] {
            query.folder = Some(bad);
            assert!(fx.search(PUB, &query, Some(&qv(5))).is_err());
        }
    }

    #[test]
    fn hidden_rows_never_returned() {
        let fx = Fx::new("hidden");
        add(&fx.w.public, "/home/x/.env", "needle", 5);
        add(&fx.w.public, "/home/x/priv/n.txt", "needle", 5); // private tier in public DB
        add(&fx.w.public, "/elsewhere/n.txt", "needle", 5);
        for mode in [SearchMode::Words, SearchMode::Meaning, SearchMode::Hybrid] {
            assert!(
                fx.search(PUB, &q("needle", mode), Some(&qv(5)))
                    .unwrap()
                    .is_empty()
            );
        }
        add(&fx.w.public, "/home/x/ok.txt", "needle", 5);
        assert_eq!(
            fx.search(PUB, &q("needle", SearchMode::Hybrid), Some(&qv(5)))
                .unwrap()[0]
                .abspath,
            "/home/x/ok.txt"
        );
    }

    #[test]
    fn words_are_whole_tokens_and_meaning_title_runs_words() {
        let fx = Fx::new("words");
        add(&fx.w.public, "/home/x/a.txt", "running fast", 1);
        assert!(
            fx.search(PUB, &q("run", SearchMode::Words), None)
                .unwrap()
                .is_empty()
        );
        let phrase = q("\"running fast\"", SearchMode::Words);
        assert_eq!(fx.search(PUB, &phrase, None).unwrap().len(), 1);
        let mut t = q("a.txt", SearchMode::Words);
        t.field = Field::Title;
        assert_eq!(fx.search(PUB, &t, None).unwrap().len(), 1);
        t.mode = SearchMode::Meaning; // title field: words over files_fts, no vectors
        assert_eq!(fx.search(PUB, &t, Some(&qv(1))).unwrap().len(), 1);
        // meaning with field=any and no query vector: nothing, and no words fallback
        assert!(
            fx.search(PUB, &q("running", SearchMode::Meaning), None)
                .unwrap()
                .is_empty()
        );
        // hybrid without a vector is words only
        assert_eq!(
            fx.search(PUB, &q("running", SearchMode::Hybrid), None)
                .unwrap()
                .len(),
            1
        );
        // limit 0 clamps to 1, huge limit to 50
        let mut l = q("running", SearchMode::Words);
        l.limit = 0;
        assert_eq!(fx.search(PUB, &l, None).unwrap().len(), 1);
        l.limit = usize::MAX;
        assert_eq!(fx.search(PUB, &l, None).unwrap().len(), 1);
    }

    #[test]
    fn query_text_is_sanitised() {
        assert_eq!(fts_expr("a\0b \u{7}c"), Some("\"ab\" OR \"c\"".into()));
        assert_eq!(fts_expr("x \"y z"), Some("\"x\" OR \"y\" OR \"z\"".into()));
        assert_eq!(
            fts_expr("x \"y z\" w"),
            Some("\"x\" OR \"y z\" OR \"w\"".into())
        );
        assert_eq!(fts_expr("\0\""), None);
    }

    #[test]
    fn rrf_orders_and_merges_two_words_lists() {
        assert_eq!(
            rrf(&[vec!["a", "b", "c"], vec!["b", "c", "a"]]),
            vec!["b", "a", "c"]
        );
        let out = rrf(&[vec!["p1", "p2"], vec!["v1", "v2"], vec!["v1", "p1"]]);
        assert_eq!(out, vec!["v1", "p1", "p2", "v2"]);
    }

    #[test]
    fn one_words_list_per_db_decides_fusion() {
        // Public A matches chunk text and title; private B chunk text only; merged meaning
        // order is [B, A]. Two words lists for A would push it above B; one list does not.
        let fx = Fx::new("ab");
        add_titled(&fx.w.public, "/home/x/a.txt", "alpha doc", "alpha", 4);
        add_titled(
            fx.w.private.as_ref().unwrap(),
            "/home/x/priv/b.txt",
            "other",
            "alpha",
            5,
        );
        let mut query = q("alpha", SearchMode::Hybrid);
        query.limit = 10;
        let hits = fx.search(StoreMode::Both, &query, Some(&qv(5))).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].abspath, "/home/x/priv/b.txt");
        assert_eq!(hits[1].abspath, "/home/x/a.txt");
    }

    #[test]
    fn both_mode_merges_and_hides_rows_outside_roots() {
        let fx = Fx::new("both");
        add(&fx.w.public, "/home/x/pub.txt", "shared", 5);
        let p = fx.w.private.as_ref().unwrap();
        add(p, "/home/x/priv/sec.txt", "shared", 6);
        add(p, "/elsewhere/leak.txt", "shared", 6);
        let mut query = q("shared", SearchMode::Hybrid);
        query.limit = 10;
        let hits = fx.search(StoreMode::Both, &query, Some(&qv(6))).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].src, 1);
        assert!(hits.iter().all(|h| h.abspath != "/elsewhere/leak.txt"));
    }

    #[test]
    fn public_search_checks_the_gate_itself() {
        let fx = Fx::new("gate");
        add(&fx.w.public, "/home/x/a.txt", "needle", 5);
        let query = q("needle", SearchMode::Words);
        assert_eq!(fx.search(PUB, &query, None).unwrap().len(), 1);
        fx.w.public.execute("DELETE FROM validated", []).unwrap(); // unvalidated paths
        let err = fx.search(PUB, &query, None).unwrap_err();
        assert_eq!(err.to_string(), crate::auth::PUBLIC_CHECKING);
        assert_eq!(fx.search(StoreMode::Both, &query, None).unwrap().len(), 1); // private never closes
        // a config_error root recorded in the DB closes the public tools too
        let h = path_hash(Path::new("/home/x")).unwrap();
        let hp = path_hash(Path::new("/home/x/priv")).unwrap();
        for x in [h, hp] {
            fx.w.public
                .execute(
                    "INSERT INTO validated VALUES(?, 0)",
                    [x.as_bytes().as_slice()],
                )
                .unwrap();
        }
        assert_eq!(fx.search(PUB, &query, None).unwrap().len(), 1);
        fx.w.public
            .execute(
                "INSERT INTO roots(name, state) VALUES('h', 'config_error')",
                [],
            )
            .unwrap();
        assert!(fx.search(PUB, &query, None).is_err());
    }
}
