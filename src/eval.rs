//! `llm-index eval-recall` (cargo feature `eval`): hybrid recall@5 of the labelled queries on
//! an indexed fixture, with the stored int8 vectors and with float32 vectors recomputed in
//! memory. Test-only.

use std::collections::HashMap;
use std::error::Error;
use std::path::Path;

use serde::Deserialize;

use crate::config;
use crate::db::{Mode, Store};
use crate::embed::{self, DIM, Embedder};
use crate::rank::{self, Field, Hit, Query, SearchMode};

type Res<T> = Result<T, Box<dyn Error>>;
const MIN_RECALL: f64 = 0.8;
const MAX_DROP: f64 = 0.05;

#[derive(Deserialize)]
struct Queries {
    query: Vec<Q>,
}

#[derive(Deserialize)]
struct Q {
    text: String,
    field: Option<String>,
    /// Relative to the first root.
    folder: Option<String>,
    #[serde(rename = "type")]
    file_type: Option<String>,
    /// Unix seconds.
    after: Option<i64>,
    before: Option<i64>,
    /// Relative paths below the first root; one of them in the top 5 counts.
    expect: Vec<String>,
}

struct Chunk {
    hit: Hit,
    ty: String,
    mtime_ns: i64,
    vec: [f32; DIM],
}

pub fn run(config_path: &Path, queries: &Path) -> Res<()> {
    let config = config::load(config_path)?;
    let root = config.roots.first().ok_or("no root")?.path.clone();
    let qs: Queries = toml::from_str(&std::fs::read_to_string(queries)?)?;
    let store = Store::open(&config.state_dir, Mode::Both)?;
    store.set_config(config);
    let model = crate::model_dir();
    let emb = Embedder::open(Path::new(model), embed::Mode::Query)?;
    let _snap = store.begin()?;

    // every chunk, re-embedded in float32 with the indexer's embedding input
    let mut chunks = Vec::new();
    for (src, conn) in [(0u8, Some(&store.public)), (1, store.private.as_ref())] {
        let Some(conn) = conn else { continue };
        let mut stmt = conn.prepare(
            "SELECT f.id, c.id, f.abspath, f.relpath, f.type, f.mtime_ns, c.heading_or_symbol, c.text
             FROM chunks c JOIN files f ON f.id = c.file_id WHERE visible(f.abspath) IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                Hit {
                    src,
                    file_id: r.get(0)?,
                    chunk_id: Some(r.get(1)?),
                    abspath: r.get(2)?,
                },
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?.unwrap_or_default(),
            ))
        })?;
        for row in rows {
            let (hit, rel, ty, mtime_ns, sym, text) = row?;
            let input = embed::input(&rel, sym.as_deref(), &text);
            if let Some(vec) = emb.embed_f32(&input)? {
                chunks.push(Chunk {
                    hit,
                    ty,
                    mtime_ns,
                    vec,
                });
            }
        }
    }

    let (mut ok8, mut ok32) = (0usize, 0usize);
    let mut fails = HashMap::new();
    for q in &qs.query {
        let folder = q
            .folder
            .as_ref()
            .map(|f| root.join(f).display().to_string());
        let field = match q.field.as_deref() {
            Some("title") => Field::Title,
            Some("path") => Field::Path,
            _ => Field::Any,
        };
        let mk = |mode, limit| Query {
            text: &q.text,
            mode,
            field,
            folder: folder.as_deref(),
            file_type: q.file_type.as_deref(),
            after_ns: q.after.map(|s| s * 1_000_000_000),
            before_ns: q.before.map(|s| s * 1_000_000_000),
            limit,
        };
        let expect: Vec<String> = q
            .expect
            .iter()
            .map(|e| root.join(e).display().to_string())
            .collect();
        let found = |hits: &[Hit]| hits.iter().take(5).any(|h| expect.contains(&h.abspath));

        let v8 = (field == Field::Any)
            .then(|| emb.embed(&q.text))
            .transpose()?
            .flatten();
        if found(&rank::search_in_snapshot(
            &store,
            &mk(SearchMode::Hybrid, 5),
            v8.as_ref(),
        )?) {
            ok8 += 1;
        } else {
            fails.insert(q.text.clone(), "int8");
        }

        // float32: the words list fused with a float cosine meaning list
        let mut lists = vec![rank::search_in_snapshot(
            &store,
            &mk(SearchMode::Words, 20),
            None,
        )?];
        if let (Field::Any, Some(qv)) = (field, emb.embed_f32(&q.text)?) {
            let in_folder = |p: &str| {
                folder
                    .as_ref()
                    .is_none_or(|f| p == f || p.starts_with(&format!("{f}/")))
            };
            let mut scored: Vec<(f32, &Hit)> = chunks
                .iter()
                .filter(|c| in_folder(&c.hit.abspath))
                .filter(|c| q.file_type.as_ref().is_none_or(|t| *t == c.ty))
                .filter(|c| q.after.is_none_or(|a| c.mtime_ns >= a * 1_000_000_000))
                .filter(|c| q.before.is_none_or(|b| c.mtime_ns <= b * 1_000_000_000))
                .map(|c| (qv.iter().zip(&c.vec).map(|(a, b)| a * b).sum(), &c.hit))
                .collect();
            scored.sort_by(|a, b| b.0.total_cmp(&a.0));
            lists.push(scored.into_iter().take(20).map(|s| s.1.clone()).collect());
        }
        if found(&rank::rrf(&lists)) {
            ok32 += 1;
        }
    }
    let n = qs.query.len() as f64;
    let (r8, r32) = (ok8 as f64 / n, ok32 as f64 / n);
    println!(
        "queries={} recall@5 int8={r8:.3} float32={r32:.3}",
        qs.query.len()
    );
    for (q, which) in &fails {
        println!("missed ({which}): {q}");
    }
    if r8 < MIN_RECALL {
        return Err(format!("int8 hybrid recall@5 {r8:.3} is below {MIN_RECALL}").into());
    }
    if r8 < r32 - MAX_DROP {
        return Err(format!("int8 recall {r8:.3} is more than {MAX_DROP} below float32 {r32:.3}: use the float32 fallback").into());
    }
    Ok(())
}
