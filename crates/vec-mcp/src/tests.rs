use super::*;
use vec_embed::Embedder;
use vec_store::{Store, StoreOptions};

const DIM: usize = 8;

struct Fixture {
    dir: tempfile::TempDir,
    server: VecServer,
}

impl Fixture {
    fn file(&self, name: &str) -> String {
        self.dir
            .path()
            .join("files")
            .join(name)
            .display()
            .to_string()
    }
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("files")).unwrap();
    for (name, word) in [("a.txt", "alpha"), ("b.txt", "beta")] {
        let text: String = (0..30).map(|i| format!("{word} line {i}\n")).collect();
        std::fs::write(root.join("files").join(name), text).unwrap();
    }
    std::fs::write(root.join("secret.txt"), "not indexed\n").unwrap();
    let cfg = Config::from_toml(&format!(
        "[embed]\nmodel = \"stub\"\n[index]\ninclude_paths = [{:?}]\ngitignore = false\n\
         [database]\ndb_path = {:?}\n[daemon]\nsocket = {:?}\n",
        root.join("files").display().to_string(),
        root.join("vec.db").display().to_string(),
        root.join("none.sock").display().to_string(),
    ))
    .unwrap();
    let mut store = Store::open(
        &cfg.database.db_path,
        &StoreOptions::from_config(&cfg.database, false),
    )
    .unwrap();
    vec_index::run_updatedb(&mut store, &Embedder::stub(DIM), &cfg, false, None, |_| {}).unwrap();
    let server = VecServer::with_searcher(Searcher::with_embedder(cfg, Embedder::stub(DIM)));
    Fixture { dir, server }
}

/// The text of a tool result, and whether it is an error result.
fn text(r: &CallToolResult) -> (String, bool) {
    let t = r
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default();
    (t, r.is_error == Some(true))
}

#[tokio::test(flavor = "multi_thread")]
async fn search_accepts_several_queries_and_excludes() {
    let fx = fixture();
    let p = SearchParams {
        query: Some("alpha".into()),
        queries: Some(vec!["beta".into()]),
        exclude: Some(vec![fx.file("b.txt")]),
        min_score: Some(-1.0),
        ..SearchParams::default()
    };
    let (out, err) = text(&fx.server.search(Parameters(p)).await.unwrap());
    assert!(!err, "{out}");
    let items: serde_json::Value = serde_json::from_str(&out).unwrap();
    let items = items.as_array().unwrap();
    assert!(!items.is_empty());
    assert!(items.iter().all(|i| i["path"] == fx.file("a.txt")), "{out}");
    assert!(items[0]["snippet"].is_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn search_without_a_query_is_an_error_result() {
    let fx = fixture();
    let (out, err) = text(
        &fx.server
            .search(Parameters(SearchParams::default()))
            .await
            .unwrap(),
    );
    assert!(err && out.contains("no query"), "{out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn context_reads_only_indexed_files() {
    let fx = fixture();
    let ok = ContextParams {
        file_path: fx.file("a.txt"),
        line: 2,
        window: Some(1),
    };
    let (out, err) = text(&fx.server.context(Parameters(ok)).await.unwrap());
    assert!(!err && out.contains(">     2: alpha line 1"), "{out}");

    for path in [
        fx.dir.path().join("secret.txt").display().to_string(),
        "/dev/zero".to_owned(),
    ] {
        let bad = ContextParams {
            file_path: path,
            line: 1,
            window: None,
        };
        let (out, err) = text(&fx.server.context(Parameters(bad)).await.unwrap());
        assert!(err && out.contains("not in the index"), "{out}");
    }

    let past = ContextParams {
        file_path: fx.file("a.txt"),
        line: 9988,
        window: None,
    };
    let (out, err) = text(&fx.server.context(Parameters(past)).await.unwrap());
    assert!(err && out.contains("outside"), "{out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn similar_and_explain_work_on_a_location() {
    let fx = fixture();
    let p = SimilarParams {
        file_path: fx.file("a.txt"),
        line: 1,
        limit: Some(5),
    };
    let (out, err) = text(&fx.server.similar(Parameters(p)).await.unwrap());
    assert!(!err, "{out}");
    assert!(
        serde_json::from_str::<serde_json::Value>(&out)
            .unwrap()
            .is_array()
    );

    let p = LocationParams {
        file_path: fx.file("a.txt"),
        line: 1,
    };
    let (out, err) = text(&fx.server.explain(Parameters(p)).await.unwrap());
    assert!(!err, "{out}");
    let e: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(!e["chunks"].as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn index_status_reports_the_model() {
    let fx = fixture();
    let (out, err) = text(
        &fx.server
            .index_status(Parameters(IndexStatusParams {
                probe_daemon: false,
            }))
            .await
            .unwrap(),
    );
    assert!(!err, "{out}");
    let s: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(s["files"], 2);
    assert_eq!(s["index_model"]["dim"], DIM);
    // The embedding runtime and canary recorded at index time.
    assert!(s["runtime_last_write"].is_string(), "{out}");
    assert!(s["runtime_first_write"].is_string(), "{out}");
    assert!(s["canary"]["text"].is_string(), "{out}");
}
