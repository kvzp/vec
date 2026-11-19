//! Ranking of search results: merge per-query hits, boost by path words,
//! order. One implementation for the CLI and the MCP server.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use vec_core::config::SearchConfig;
use vec_store::SearchResult;

/// Merge the hits of several queries (one list per query) into at most
/// `limit` results.
///
/// - A chunk found by several queries (same path and start line) keeps its
///   best score.
/// - Each distinct query word of at least `[search] path_boost_min_word_chars`
///   characters that occurs in the result's path below its include path adds
///   `[search] path_boost`; a boosted score never exceeds 1.0.
/// - Results are ordered by score (descending), then path and line, so the
///   order is deterministic.
#[must_use]
pub fn rank(
    cfg: &SearchConfig,
    include_paths: &[PathBuf],
    queries: &[String],
    per_query: Vec<Vec<SearchResult>>,
    limit: usize,
) -> Vec<SearchResult> {
    let mut merged: HashMap<(PathBuf, usize), SearchResult> = HashMap::new();
    for hit in per_query.into_iter().flatten() {
        let key = (hit.path.clone(), hit.start_line);
        match merged.get(&key) {
            Some(old) if old.score >= hit.score => {}
            _ => {
                merged.insert(key, hit);
            }
        }
    }
    let mut results: Vec<SearchResult> = merged.into_values().collect();

    let words = boost_words(queries, cfg.path_boost_min_word_chars);
    if cfg.path_boost > 0.0 && !words.is_empty() {
        for r in &mut results {
            let matches = path_word_matches(&r.path, include_paths, &words);
            if matches > 0 {
                r.score = (r.score + cfg.path_boost * matches as f32).min(1.0);
            }
        }
    }

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.start_line.cmp(&b.start_line))
    });
    results.truncate(limit);
    results
}

/// Distinct lowercase words of `queries` with at least `min_chars` characters.
fn boost_words(queries: &[String], min_chars: usize) -> Vec<String> {
    let mut words: Vec<String> = vec_core::snippet::query_words(queries)
        .into_iter()
        .filter(|w| w.chars().count() >= min_chars)
        .collect();
    words.sort_unstable();
    words.dedup();
    words
}

/// How many of `words` occur in `path` below the longest include path that
/// contains it (the whole path when none does), case-insensitively.
fn path_word_matches(path: &Path, include_paths: &[PathBuf], words: &[String]) -> usize {
    let below = include_paths
        .iter()
        .filter_map(|root| path.strip_prefix(root).ok())
        .min_by_key(|rel| rel.as_os_str().len())
        .unwrap_or(path);
    let lower = below.to_string_lossy().to_lowercase();
    words.iter().filter(|w| lower.contains(w.as_str())).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SearchConfig {
        let mut s = vec_core::config::Config::defaults().unwrap().search;
        s.path_boost = 0.1;
        s.path_boost_min_word_chars = 3;
        s
    }

    fn hit(path: &str, line: usize, score: f32) -> SearchResult {
        SearchResult {
            path: PathBuf::from(path),
            byte_offset: 0,
            byte_end: 1,
            start_line: line,
            end_line: line + 1,
            score,
        }
    }

    fn q(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[test]
    fn merge_keeps_the_best_score_per_chunk() {
        let out = rank(
            &cfg(),
            &[],
            &q(&["zz"]),
            vec![
                vec![hit("/a", 1, 0.5)],
                vec![hit("/a", 1, 0.7), hit("/b", 1, 0.6)],
            ],
            10,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(
            (out[0].path.as_path(), out[0].score),
            (Path::new("/a"), 0.7)
        );
    }

    #[test]
    fn path_boost_counts_words_below_the_include_path() {
        // "home" is in the include path: it must not boost.
        let roots = [PathBuf::from("/home/me")];
        let out = rank(
            &cfg(),
            &roots,
            &q(&["home auth"]),
            vec![vec![
                hit("/home/me/auth.rs", 1, 0.5),
                hit("/home/me/x.rs", 1, 0.55),
            ]],
            10,
        );
        assert_eq!(out[0].path, Path::new("/home/me/auth.rs"));
        assert!((out[0].score - 0.6).abs() < 1e-6);
        assert!((out[1].score - 0.55).abs() < 1e-6);
    }

    #[test]
    fn short_words_do_not_boost_and_scores_stay_at_most_one() {
        let out = rank(
            &cfg(),
            &[],
            &q(&["a in", "src main"]),
            vec![vec![hit("/src/main.rs", 1, 0.95), hit("/in/a.rs", 1, 0.9)]],
            10,
        );
        assert_eq!(out[0].score, 1.0);
        assert!((out[1].score - 0.9).abs() < 1e-6, "short words boosted");
    }

    #[test]
    fn order_is_deterministic_and_limited() {
        let out = rank(
            &cfg(),
            &[],
            &q(&["zz"]),
            vec![vec![
                hit("/b", 1, 0.5),
                hit("/a", 2, 0.5),
                hit("/a", 1, 0.5),
            ]],
            2,
        );
        let keys: Vec<_> = out.iter().map(|r| (r.path.clone(), r.start_line)).collect();
        assert_eq!(keys, [(PathBuf::from("/a"), 1), (PathBuf::from("/a"), 2)]);
    }
}
