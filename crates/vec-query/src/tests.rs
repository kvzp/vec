use super::*;
use vec_store::StoreOptions;

/// Dimension of the stub vectors used by the tests.
const DIM: usize = 8;

struct Fixture {
    dir: tempfile::TempDir,
    cfg: Config,
}

impl Fixture {
    fn root(&self) -> &Path {
        self.dir.path()
    }
    fn file(&self, name: &str) -> PathBuf {
        self.root().join("files").join(name)
    }
    fn searcher(&self) -> Searcher {
        Searcher::with_embedder(self.cfg.clone(), Embedder::stub(DIM))
    }
}

fn write_lines(path: &Path, n: usize, word: &str) {
    let text: String = (0..n).map(|i| format!("{word} line {i}\n")).collect();
    std::fs::write(path, text).unwrap();
}

fn config(root: &Path, model: &str) -> Config {
    let text = format!(
        "[embed]\nmodel = {model:?}\n\
         [index]\ninclude_paths = [{files:?}]\ngitignore = false\n\
         [database]\ndb_path = {db:?}\n\
         [daemon]\nsocket = {sock:?}\n",
        files = root.join("files").display().to_string(),
        db = root.join("vec.db").display().to_string(),
        sock = root.join("no-daemon.sock").display().to_string(),
    );
    Config::from_toml(&text).unwrap()
}

/// Files a.txt, b.txt, sub/c.txt indexed with the stub embedder.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("files/sub")).unwrap();
    let cfg = config(dir.path(), "stub");
    let fx = Fixture { dir, cfg };
    write_lines(&fx.file("a.txt"), 30, "alpha");
    write_lines(&fx.file("b.txt"), 30, "beta");
    write_lines(&fx.file("sub/c.txt"), 30, "gamma");
    let mut store = Store::open(
        &fx.cfg.database.db_path,
        &StoreOptions::from_config(&fx.cfg.database, false),
    )
    .unwrap();
    vec_index::run_updatedb(
        &mut store,
        &Embedder::stub(DIM),
        &fx.cfg,
        false,
        None,
        |_| {},
    )
    .unwrap();
    fx
}

fn request(queries: &[&str]) -> SearchRequest {
    SearchRequest {
        queries: queries.iter().map(|q| (*q).to_owned()).collect(),
        min_score: Some(-1.0),
        ..SearchRequest::default()
    }
}

fn paths(results: &[SearchResult]) -> Vec<PathBuf> {
    results.iter().map(|r| r.path.clone()).collect()
}

#[test]
fn search_uses_a_read_only_store_and_finds_files() {
    let fx = fixture();
    let s = fx.searcher();
    let store = s.open_store().unwrap();
    let hits = s.search(&store, &request(&["alpha"])).unwrap();
    assert!(!hits.is_empty());
    // Read-only: a write is refused.
    let mut store = store;
    assert!(store.set_meta("k", "v").is_err());
}

#[test]
fn filters_apply_before_the_limit() {
    let fx = fixture();
    let s = fx.searcher();
    let store = s.open_store().unwrap();
    let mut req = request(&["alpha", "beta"]);
    req.limit = Some(2);
    req.exclude = vec![fx.file("a.txt"), fx.file("b.txt")];
    let hits = s.search(&store, &req).unwrap();
    // Only sub/c.txt remains, and it has a chunk for each query's top 2.
    assert!(!hits.is_empty());
    assert!(
        hits.iter().all(|h| h.path == fx.file("sub/c.txt")),
        "{hits:?}"
    );

    let mut req = request(&["x"]);
    req.path_filter = Some(fx.file("sub"));
    let hits = s.search(&store, &req).unwrap();
    assert_eq!(paths(&hits), [fx.file("sub/c.txt")]);
}

#[test]
fn query_count_and_blank_queries_are_checked() {
    let fx = fixture();
    let s = fx.searcher();
    let store = s.open_store().unwrap();
    assert!(s.search(&store, &request(&[])).is_err());
    assert!(s.search(&store, &request(&["  "])).is_err());
    let many: Vec<String> = (0..=fx.cfg.search.max_queries)
        .map(|i| i.to_string())
        .collect();
    let req = SearchRequest {
        queries: many,
        ..SearchRequest::default()
    };
    let err = s.search(&store, &req).unwrap_err().to_string();
    assert!(err.contains("max_queries"), "{err}");
}

#[test]
fn a_renamed_model_asks_for_a_full_rebuild() {
    let fx = fixture();
    let cfg = config(fx.root(), "another-model");
    let s = Searcher::with_embedder(cfg, Embedder::stub(DIM));
    let store = s.open_store().unwrap();
    let err = format!("{:#}", s.search(&store, &request(&["q"])).unwrap_err());
    assert!(err.contains("vec updatedb --full"), "{err}");
    assert!(!err.contains("  "), "{err}");
}

#[test]
fn an_embedder_of_another_dimension_is_refused() {
    let fx = fixture();
    let s = Searcher::with_embedder(fx.cfg.clone(), Embedder::stub(DIM * 2));
    let store = s.open_store().unwrap();
    let err = format!("{:#}", s.search(&store, &request(&["q"])).unwrap_err());
    assert!(err.contains("vec updatedb --full"), "{err}");
}

#[test]
fn an_empty_index_loads_no_model() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("files")).unwrap();
    let cfg = config(dir.path(), "not-installed");
    Store::open(
        &cfg.database.db_path,
        &StoreOptions::from_config(&cfg.database, false),
    )
    .unwrap();
    // Searcher::new would fail to load "not-installed" if it tried.
    let s = Searcher::new(cfg);
    let store = s.open_store().unwrap();
    assert!(s.search(&store, &request(&["q"])).unwrap().is_empty());
}

#[test]
fn similar_skips_the_chunk_itself() {
    let fx = fixture();
    let s = fx.searcher();
    let store = s.open_store().unwrap();
    let a = fx.file("a.txt");
    let hits = s.similar(&store, &a, 1, Some(10)).unwrap();
    assert!(
        !hits
            .iter()
            .any(|h| h.path == a && h.start_line <= 1 && 1 <= h.end_line)
    );
    assert!(s.similar(&store, &a, 10_000, None).is_err());
}

#[test]
fn explain_lists_covering_chunks() {
    let fx = fixture();
    let s = fx.searcher();
    let store = s.open_store().unwrap();
    let e = s.explain(&store, &fx.file("a.txt"), 3).unwrap();
    assert!(!e.chunks.is_empty());
    assert!(e.indexed_hash.is_some());
    assert_eq!(e.changed_since_indexed, Some(false));
    let err = s.explain(&store, &fx.file("nope.txt"), 3).unwrap_err();
    assert!(format!("{err:#}").contains("no such file"), "{err:#}");
    // An unindexed file that exists: nothing indexed.
    let other = fx.root().join("files/unindexed.txt");
    std::fs::write(&other, "x\n").unwrap();
    let none = s.explain(&store, &other, 1).unwrap();
    assert!(none.chunks.is_empty() && none.indexed_hash.is_none());
}

#[test]
fn annotate_survives_a_shrunk_file() {
    let fx = fixture();
    let s = fx.searcher();
    let store = s.open_store().unwrap();
    let hits = s.search(&store, &request(&["gamma"])).unwrap();
    std::fs::write(fx.file("sub/c.txt"), "now\nshort\n").unwrap();
    std::fs::write(fx.file("a.txt"), "é".repeat(5)).unwrap();
    let out = s.annotate(&hits, &["gamma".to_owned()], Some(3));
    for a in &out {
        if a.path == fx.file("sub/c.txt") {
            assert!(a.line <= 2);
            assert!(a.snippet.as_deref().is_some_and(|t| t.contains("short")));
        }
    }
}

#[test]
fn annotate_finds_the_best_line() {
    let fx = fixture();
    let s = fx.searcher();
    let mut text: String = (0..30).map(|i| format!("filler {i}\n")).collect();
    text.push_str("the needle is here\n");
    std::fs::write(fx.file("b.txt"), &text).unwrap();
    let hit = SearchResult {
        path: fx.file("b.txt"),
        byte_offset: 0,
        byte_end: text.len(),
        start_line: 1,
        end_line: 31,
        score: 0.5,
    };
    let out = s.annotate(&[hit], &["needle".to_owned()], None);
    assert_eq!(out[0].line, 31);
    assert_eq!(out[0].snippet, None);
}

#[test]
fn read_indexed_refuses_files_outside_the_index() {
    let fx = fixture();
    let s = fx.searcher();
    let store = s.open_store().unwrap();
    assert!(s.read_indexed(&store, &fx.file("a.txt")).is_ok());
    let outside = fx.root().join("secret.txt");
    std::fs::write(&outside, "secret").unwrap();
    let err = s.read_indexed(&store, &outside).unwrap_err().to_string();
    assert!(err.contains("not in the index"), "{err}");
}

#[test]
fn context_refuses_lines_outside_the_file() {
    let p = Path::new("/f");
    assert_eq!(context("a\nb\nc", p, 2, 0).unwrap(), ">     2: b");
    assert!(context("a\nb", p, 9988, 10).is_err()); // the old panic
    assert!(context("a", p, 0, 1).is_err());
}

#[test]
fn parse_location_cases() {
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(
        parse_location("src/x.rs:42").unwrap(),
        (cwd.join("src/x.rs"), 42)
    );
    assert_eq!(
        parse_location("/a:b/c:7").unwrap(),
        (PathBuf::from("/a:b/c"), 7)
    );
    assert_eq!(
        parse_location("/a/../b:1").unwrap(),
        (PathBuf::from("/b"), 1)
    );
    for bad in ["x.rs", ":3", "x.rs:0", "x.rs:-1", "x.rs:abc", "x.rs:"] {
        assert!(parse_location(bad).is_err(), "{bad}");
    }
}

#[test]
fn status_reports_index_watcher_and_daemon() {
    let fx = fixture();
    let s = fx.searcher();
    let store = s.open_store().unwrap();
    let r = status::status(&fx.cfg, &store, false).unwrap();
    assert_eq!(r.files, 3);
    assert_eq!(r.schema_version, r.supported_schema_version);
    assert_eq!(r.index_model.as_ref().map(|m| m.dim), Some(DIM));
    assert!(!r.model_mismatch);
    assert!(r.last_updatedb.is_some());
    assert_eq!(r.watcher, None);
    // Regression (review 2026-11-19 #10): plain status does not connect to
    // the daemon socket (that would start a socket-activated daemon).
    assert_eq!(
        r.daemon,
        status::DaemonStatus::NotProbed {
            socket_exists: false
        }
    );
    #[cfg(unix)]
    assert_eq!(
        status::status(&fx.cfg, &store, true).unwrap().daemon,
        status::DaemonStatus::NotRunning
    );
}

/// Regression (review 2026-11-19 #7): a daemon that cannot be used made
/// every search fail. Here connecting fails with an error that is not "no
/// daemon" (the socket path is too long for a Unix socket address), as a
/// busy, hung or outdated daemon would: the search embeds in-process, and
/// the searcher stops trying the daemon.
#[cfg(unix)]
#[test]
fn unusable_daemon_falls_back_to_in_process_embedding() {
    let mut fx = fixture();
    fx.cfg.daemon.socket = fx.root().join(format!("{}.sock", "d".repeat(200)));
    let s = fx.searcher();
    let store = s.open_store().unwrap();
    match vec_daemon::client::embed(&fx.cfg.daemon, &store.model().unwrap().unwrap(), &["x"]) {
        Err(_) => {}
        // A sandbox that forbids Unix sockets reports "no daemon" (EPERM
        // from socket(2)) before the address is checked: nothing to test.
        Ok(None) => return,
        Ok(Some(_)) => panic!("no daemon can answer at this path"),
    }
    let hits = s.search(&store, &request(&["alpha"])).unwrap();
    assert!(!hits.is_empty());
    assert!(s.daemon_failed.load(Ordering::Relaxed));
    // Later queries go in-process directly.
    assert!(!s.search(&store, &request(&["beta"])).unwrap().is_empty());
}

#[test]
fn daemon_warning_is_one_line() {
    let e = anyhow::anyhow!("first\nsecond").context("outer");
    let w = daemon_warning(Path::new("/run/vec/embed.sock"), &e);
    assert!(!w.contains('\n'), "{w}");
    assert!(w.contains("in-process") && w.contains("second"), "{w}");
}

/// Regression (review 2026-11-19 #12): `similar` and `explain` revealed
/// what the index holds for a file the caller cannot read.
#[cfg(unix)]
#[test]
fn similar_and_explain_require_read_access() {
    use std::os::unix::fs::PermissionsExt;
    let fx = fixture();
    let s = fx.searcher();
    let store = s.open_store().unwrap();
    let a = fx.file("a.txt");
    assert!(s.similar(&store, &a, 1, None).is_ok());
    assert!(s.explain(&store, &a, 1).is_ok());
    std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o000)).unwrap();
    if util::can_read(&a) {
        return; // root reads everything: nothing to check
    }
    for err in [
        s.similar(&store, &a, 1, None).unwrap_err(),
        s.explain(&store, &a, 1).unwrap_err(),
    ] {
        assert!(format!("{err:#}").contains("permission denied"), "{err:#}");
    }
}

// ---------------------------------------------------------------------------
// Real model (run with VEC_TEST_MODEL=<model.onnx> cargo test -- --ignored)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs a real ONNX model in VEC_TEST_MODEL"]
fn real_model_search_ranks_the_matching_file_first_and_loads_once() {
    let model = std::env::var_os("VEC_TEST_MODEL")
        .expect("set VEC_TEST_MODEL to an ONNX model file with tokenizer.json beside it");
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("files")).unwrap();
    // [embed] model = the absolute model path: load_embedder resolves it.
    let cfg = config(dir.path(), &PathBuf::from(&model).display().to_string());
    let topics = [
        (
            "bake.txt",
            "Preheat the oven, mix flour, sugar and eggs, then bake the cake",
        ),
        (
            "tcp.txt",
            "The TCP handshake sends SYN, SYN-ACK and ACK packets over the network",
        ),
        (
            "tax.txt",
            "File your income tax return before the deadline to avoid a penalty",
        ),
    ];
    for (name, sentence) in topics {
        let text: String = (0..12).map(|i| format!("{sentence} ({i}).\n")).collect();
        std::fs::write(dir.path().join("files").join(name), text).unwrap();
    }
    let embedder = vec_core::load_embedder(&cfg).unwrap();
    let mut store = Store::open(
        &cfg.database.db_path,
        &StoreOptions::from_config(&cfg.database, false),
    )
    .unwrap();
    vec_index::run_updatedb(&mut store, &embedder, &cfg, false, None, |_| {}).unwrap();
    drop(store);

    let s = Searcher::new(cfg.clone());
    let store = s.open_store().unwrap();
    for (query, expected) in [
        ("how do I make a dessert in the oven", "bake.txt"),
        ("network connection packets", "tcp.txt"),
        ("paying taxes on time", "tax.txt"),
    ] {
        let hits = s.search(&store, &request(&[query])).unwrap();
        assert_eq!(
            hits[0].path,
            dir.path().join("files").join(expected),
            "{query}"
        );
    }
    let first = s.embedder.get().map(std::ptr::from_ref);
    // Several queries at once: merged, and the same loaded model is reused.
    let hits = s
        .search(&store, &request(&["oven dessert", "network packets"]))
        .unwrap();
    let top: Vec<_> = hits.iter().take(2).map(|h| h.path.clone()).collect();
    assert!(top.contains(&dir.path().join("files/bake.txt")), "{top:?}");
    assert!(top.contains(&dir.path().join("files/tcp.txt")), "{top:?}");
    assert_eq!(s.embedder.get().map(std::ptr::from_ref), first);

    // The query vector equals the indexed one for the same text.
    let v = s.embed(&store, &["same text"]).unwrap();
    let direct = embedder.embed_one("same text").unwrap();
    let cos: f32 = v[0].iter().zip(&direct).map(|(a, b)| a * b).sum();
    assert!(cos > 0.9999, "{cos}");
}
