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
fn fts_expr(text: &str) -> Option<String> {
    let mut terms = Vec::new();
    for (i, part) in text.split('"').enumerate() {
        if i % 2 == 1 {
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

// Cheap column filters first; `visible` last. Public server rows must be public.
fn eligible(mode: StoreMode) -> String {
    let vis = if mode == StoreMode::PublicOnly {
        "visible(f.abspath) = 'public'"
    } else {
        "visible(f.abspath) IS NOT NULL"
    };
    format!(
        "(:folder IS NULL OR f.abspath = :folder OR f.abspath LIKE :like ESCAPE '\\')
         AND (:ty IS NULL OR f.type = :ty) AND (:after IS NULL OR f.mtime_ns >= :after)
         AND (:before IS NULL OR f.mtime_ns <= :before) AND {vis}"
    )
}

fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
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
    let folder = q.folder.map(|f| f.trim_end_matches('/'));
    let like = folder.map(|f| format!("{}/%", like_escape(f)));
    let pool = pool as i64;
    let mut params: Vec<(&str, &dyn rusqlite::ToSql)> = vec![
        (":folder", &folder),
        (":like", &like),
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

fn words_legs(
    store: &Store,
    conn: &Connection,
    src: u8,
    q: &Query,
    pool: usize,
) -> Res<Vec<Vec<Hit>>> {
    let Some(expr) = fts_expr(q.text) else {
        return Ok(vec![]);
    };
    let elig = eligible(store.mode);
    let mut out = Vec::new();
    if q.field == Field::Any {
        let sql = format!(
            "SELECT f.id, c.id, f.abspath, 0 FROM chunks_fts JOIN chunks c ON c.id = chunks_fts.rowid
             JOIN files f ON f.id = c.file_id WHERE chunks_fts MATCH :m AND {elig}
             ORDER BY bm25(chunks_fts) LIMIT :pool"
        );
        out.push(run(conn, &sql, q, src, &[(":m", &expr)], pool)?);
    }
    let col = match q.field {
        Field::Any => String::new(),
        Field::Title => "title : ".into(),
        Field::Path => "path : ".into(),
    };
    let m = format!("{col}({expr})");
    let sql = format!(
        "SELECT f.id, {FIRST_CHUNK}, f.abspath, 0 FROM files_fts JOIN files f ON f.id = files_fts.rowid
         WHERE files_fts MATCH :m AND {elig} ORDER BY bm25(files_fts) LIMIT :pool"
    );
    out.push(run(conn, &sql, q, src, &[(":m", &m)], pool)?);
    Ok(out
        .into_iter()
        .map(|l| l.into_iter().map(|(h, _)| h).collect())
        .collect())
}

/// `qvec` is needed for meaning and hybrid modes with `Field::Any`.
pub fn search(store: &Store, q: &Query, qvec: Option<&[i8; DIM]>) -> Res<Vec<Hit>> {
    let pool = q.limit.max(1) * 4;
    let conns: Vec<(u8, &Connection)> = std::iter::once((0, &store.public))
        .chain(store.private.as_ref().map(|c| (1, c)))
        .collect();
    let mut lists: Vec<Vec<Hit>> = Vec::new();
    if q.mode != SearchMode::Meaning || q.field != Field::Any {
        for &(src, conn) in &conns {
            lists.extend(words_legs(store, conn, src, q, pool)?);
        }
    }
    if q.mode != SearchMode::Words && q.field == Field::Any {
        let v: Vec<u8> = qvec
            .ok_or("meaning search needs a query vector")?
            .iter()
            .map(|&x| x as u8)
            .collect();
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
    hits.truncate(q.limit);
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::tests::tmp;
    use std::collections::HashMap;

    fn vec_of(hot: usize) -> Vec<u8> {
        let mut v = vec![0u8; DIM];
        v[hot] = 100;
        v[(hot + 1) % DIM] = 20;
        v
    }

    fn setup(name: &str, mode: StoreMode) -> Store {
        let s = Store::open(&tmp(name), mode).unwrap();
        let cfg: Config = toml::from_str(
            "state_dir = \"/state\"\n[[root]]\nname=\"h\"\npath=\"/home/x\"\ntier=\"public\"\n\
             [[root]]\nname=\"p\"\npath=\"/home/x/priv\"\ntier=\"private\"\n",
        )
        .unwrap();
        s.set_state(cfg, HashMap::new()).unwrap();
        s
    }

    fn add(conn: &Connection, abspath: &str, text: &str, hot: usize) {
        conn.execute(
            "INSERT INTO files(root_id, relpath, abspath, title, type, mtime_ns) VALUES('h', ?1, ?1, ?1, 'text', 1)",
            [abspath],
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
            "INSERT INTO files_fts(rowid, title, path) VALUES(?1, ?2, ?2)",
            rusqlite::params![fid, abspath],
        )
        .unwrap();
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

    #[test]
    fn nearest_first_and_filter_before_limit() {
        let s = setup("meaning", StoreMode::PublicOnly);
        for i in 0..20 {
            add(&s.public, &format!("/home/x/near/{i}.txt"), "zzz", 5); // nearest, ineligible
        }
        add(&s.public, "/home/x/far/a.txt", "zzz", 9);
        add(&s.public, "/home/x/far/b.txt", "zzz", 200);
        let mut query = q("", SearchMode::Meaning);
        assert!(
            search(&s, &query, Some(&qv(5))).unwrap()[0]
                .abspath
                .starts_with("/home/x/near/")
        );
        query.folder = Some("/home/x/far");
        let hits = search(&s, &query, Some(&qv(8))).unwrap();
        assert_eq!(hits[0].abspath, "/home/x/far/a.txt");
        query.folder = Some("/home/x/fa"); // component-wise prefix
        assert!(search(&s, &query, Some(&qv(8))).unwrap().is_empty());
    }

    #[test]
    fn hidden_rows_never_returned() {
        let s = setup("hidden", StoreMode::PublicOnly);
        add(&s.public, "/home/x/.env", "needle", 5);
        add(&s.public, "/home/x/priv/n.txt", "needle", 5); // private tier in public DB
        add(&s.public, "/elsewhere/n.txt", "needle", 5);
        for mode in [SearchMode::Words, SearchMode::Meaning, SearchMode::Hybrid] {
            assert!(
                search(&s, &q("needle", mode), Some(&qv(5)))
                    .unwrap()
                    .is_empty()
            );
        }
        add(&s.public, "/home/x/ok.txt", "needle", 5);
        assert_eq!(
            search(&s, &q("needle", SearchMode::Hybrid), Some(&qv(5))).unwrap()[0].abspath,
            "/home/x/ok.txt"
        );
    }

    #[test]
    fn words_are_whole_tokens() {
        let s = setup("words", StoreMode::PublicOnly);
        add(&s.public, "/home/x/a.txt", "running fast", 1);
        assert!(
            search(&s, &q("run", SearchMode::Words), None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            search(&s, &q("\"running fast\"", SearchMode::Words), None)
                .unwrap()
                .len(),
            1
        );
        let mut t = q("a.txt", SearchMode::Words);
        t.field = Field::Title;
        assert_eq!(search(&s, &t, None).unwrap().len(), 1);
    }

    #[test]
    fn rrf_orders_and_merges_two_words_lists() {
        assert_eq!(
            rrf(&[vec!["a", "b", "c"], vec!["b", "c", "a"]]),
            vec!["b", "a", "c"]
        );
        // two separate words lists (one per DB) plus a merged meaning list
        let out = rrf(&[vec!["p1", "p2"], vec!["v1", "v2"], vec!["v1", "p1"]]);
        assert_eq!(out, vec!["v1", "p1", "p2", "v2"]);
    }

    #[test]
    fn private_store_merges_both_dbs() {
        let s = setup("both", StoreMode::Both);
        add(&s.public, "/home/x/pub.txt", "shared", 5);
        add(
            s.private.as_ref().unwrap(),
            "/home/x/priv/sec.txt",
            "shared",
            6,
        );
        let mut query = q("shared", SearchMode::Hybrid);
        query.limit = 10;
        let hits = search(&s, &query, Some(&qv(6))).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].src, 1);
    }
}
