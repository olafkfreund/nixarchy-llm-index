//! `llm-index index` end to end on a temp fixture: real coordinator, real workers, real DBs,
//! searched through a read-only `Store` with `rank::search`.
use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use llm_index::config;
use llm_index::db::{Mode, Store};
use llm_index::rank::{Field, Query, SearchMode, search};

fn query(text: &str) -> Query<'_> {
    Query {
        text,
        mode: SearchMode::Words,
        field: Field::Any,
        folder: None,
        file_type: None,
        after_ns: None,
        before_ns: None,
        limit: 10,
    }
}

fn count(conn: &rusqlite::Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0)
}

#[test]
fn index_makes_searchable_rows_and_keeps_tiers_apart() {
    let base = std::env::temp_dir().join(format!("llm-index-e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let (pubr, privr) = (base.join("pub"), base.join("priv"));
    fs::create_dir_all(&pubr).unwrap();
    fs::create_dir_all(&privr).unwrap();
    fs::write(
        pubr.join("garden.md"),
        "# Garden\n\nTomatoes need watering every morning.\n",
    )
    .unwrap();
    fs::write(
        privr.join("diary.md"),
        "# Diary\n\nThe zeppelin schedule is confidential.\n",
    )
    .unwrap();
    fs::write(pubr.join(".env"), "TOKEN=zeppelin\n").unwrap();
    // unsafe names: a file and a directory (with a safe file inside) are never indexed
    fs::write(pubr.join("rlo\u{202e}file.md"), "quokka in a file\n").unwrap();
    fs::create_dir_all(pubr.join("rlo\u{202e}dir")).unwrap();
    fs::write(pubr.join("rlo\u{202e}dir/inner.md"), "quokka in a dir\n").unwrap();
    fs::write(pubr.join("quokka-sibling.md"), "quokka sibling\n").unwrap();
    // the state dir sits inside a root: it must never be indexed
    let state = pubr.join("idx-state");
    let cfg = base.join("config.toml");
    fs::write(
        &cfg,
        format!(
            "state_dir = \"{}\"\n[[root]]\nname = \"pub\"\npath = \"{}\"\ntier = \"public\"\n\
             [[root]]\nname = \"priv\"\npath = \"{}\"\ntier = \"private\"\n",
            state.display(),
            pubr.display(),
            privr.display()
        ),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_llm-index"))
        .arg("--config")
        .arg(&cfg)
        .arg("index")
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let ready = loop {
        if let Ok(s) = Store::open(&state, Mode::Both) {
            let (p, q) = (&s.public, s.private.as_ref().unwrap());
            if count(p, "SELECT count(*) FROM files") >= 2
                && count(q, "SELECT count(*) FROM files") >= 1
                && count(p, "SELECT count(*) FROM validated") >= 1
            {
                break true;
            }
        }
        if start.elapsed() > Duration::from_secs(90) {
            break false;
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    let _ = child.kill();
    let _ = child.wait();
    assert!(ready, "index produced no rows in time");

    let public = Store::open(&state, Mode::PublicOnly).unwrap();
    public.set_config(config::load(&cfg).unwrap());
    {
        let _snap = public.begin().unwrap();
        public.gate().expect("roots validated");
    }
    let hits = search(&public, &query("tomatoes"), None).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(hits[0].abspath.ends_with("pub/garden.md"));
    assert!(
        search(&public, &query("zeppelin"), None)
            .unwrap()
            .is_empty()
    );
    assert!(search(&public, &query("diary"), None).unwrap().is_empty());
    let hits = search(&public, &query("quokka"), None).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(hits[0].abspath.ends_with("pub/quokka-sibling.md"));
    assert_eq!(
        count(
            &public.public,
            "SELECT count(*) FROM files WHERE relpath LIKE '%' || char(8238) || '%'"
        ),
        0
    );

    let both = Store::open(&state, Mode::Both).unwrap();
    both.set_config(config::load(&cfg).unwrap());
    let hits = search(&both, &query("zeppelin"), None).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(hits[0].abspath.ends_with("priv/diary.md"));
    // no file of the state dir and no denied file reached either database
    for conn in [&both.public, both.private.as_ref().unwrap()] {
        assert_eq!(
            count(
                conn,
                "SELECT count(*) FROM files WHERE abspath LIKE '%idx-state%'"
            ),
            0
        );
        assert_eq!(
            count(
                conn,
                "SELECT count(*) FROM files WHERE abspath LIKE '%.env'"
            ),
            0
        );
    }
    assert_eq!(
        count(
            &both.public,
            "SELECT count(*) FROM roots WHERE name = 'priv'"
        ),
        0
    );
    let _ = fs::remove_dir_all(&base);
}
