//! Best-line targeting and snippet rendering for search results, shared by
//! `vec` (CLI) and `vec serve` (MCP).
//!
//! Everything here works on possibly stale data: a result's byte offsets and
//! line numbers come from the index, while the text comes from the live file,
//! which may have been edited, shrunk or rewritten since it was indexed. So
//! nothing here panics on any input: byte offsets are clamped to the text and
//! moved back to a character boundary, and line numbers past the end of the
//! text are clamped to its last line.

use std::fmt::Write as _;
use std::ops::Range;

/// The words of `text`, lowercased: split at every character that is not
/// alphanumeric and at camelCase boundaries (`retryUpload` → `retry`,
/// `upload`; `HTTPServer` → `http`, `server`), so identifiers, paths and
/// prose split the same way.
#[must_use]
pub fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in text.split(|c: char| !c.is_alphanumeric()) {
        let chars: Vec<char> = part.chars().collect();
        let mut start = 0;
        for i in 1..chars.len() {
            let (prev, cur) = (chars[i - 1], chars[i]);
            let next_lower = chars.get(i + 1).is_some_and(|c| c.is_lowercase());
            let lower_to_upper = (prev.is_lowercase() || prev.is_numeric()) && cur.is_uppercase();
            let acronym_end = prev.is_uppercase() && cur.is_uppercase() && next_lower;
            if lower_to_upper || acronym_end {
                out.push(chars[start..i].iter().collect::<String>().to_lowercase());
                start = i;
            }
        }
        if start < chars.len() {
            out.push(chars[start..].iter().collect::<String>().to_lowercase());
        }
    }
    out
}

/// The words of a search, used to pick a result's best line and to boost
/// paths: the distinct [`tokens`] of the queries, without the configured
/// stop words (`[search] stop_words`, such as "where" or "the").
///
/// A query word matches a token of a line or path when they are equal, or
/// when both have at least `prefix_min_chars` characters and one starts with
/// the other (`upload` matches `uploads`, `embed` matches `embedding`).
/// Matching is on whole tokens, never substrings: `the` does not match
/// `TheApp`'s `machine`, nor `do` match `documents`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryTerms {
    words: Vec<String>,
    prefix_min_chars: usize,
}

impl QueryTerms {
    /// The terms of `queries`; `stop_words` are compared case-insensitively.
    #[must_use]
    pub fn new<S: AsRef<str>>(
        queries: &[S],
        stop_words: &[String],
        prefix_min_chars: usize,
    ) -> Self {
        let stop: Vec<String> = stop_words.iter().map(|w| w.to_lowercase()).collect();
        let mut words: Vec<String> = Vec::new();
        for q in queries {
            for w in tokens(q.as_ref()) {
                if !stop.contains(&w) && !words.contains(&w) {
                    words.push(w);
                }
            }
        }
        Self {
            words,
            prefix_min_chars,
        }
    }

    /// The words, in query order.
    #[must_use]
    pub fn words(&self) -> &[String] {
        &self.words
    }

    /// The same terms without the words shorter than `min_chars` characters.
    #[must_use]
    pub fn at_least(&self, min_chars: usize) -> Self {
        Self {
            words: self
                .words
                .iter()
                .filter(|w| w.chars().count() >= min_chars)
                .cloned()
                .collect(),
            prefix_min_chars: self.prefix_min_chars,
        }
    }

    /// `true` when there is no word (an empty query, or only stop words).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// How many distinct words match some token of `text`.
    #[must_use]
    pub fn count_in(&self, text: &str) -> usize {
        if self.words.is_empty() {
            return 0;
        }
        let toks = tokens(text);
        self.words
            .iter()
            .filter(|w| toks.iter().any(|t| self.matches(w, t)))
            .count()
    }

    fn matches(&self, word: &str, token: &str) -> bool {
        if word == token {
            return true;
        }
        let (short, long) = if word.len() <= token.len() {
            (word, token)
        } else {
            (token, word)
        };
        short.chars().count() >= self.prefix_min_chars && long.starts_with(short)
    }
}

/// `content[bytes]`, with both ends clamped to `content` and moved back to a
/// character boundary (an empty string when the range is empty or reversed).
#[must_use]
pub fn clamped_slice(content: &str, bytes: Range<usize>) -> &str {
    let end = floor_char_boundary(content, bytes.end.min(content.len()));
    let start = floor_char_boundary(content, bytes.start.min(end));
    &content[start..end]
}

/// The largest character boundary of `s` that is `<= i` (`i <= s.len()`).
fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Number of lines of `content` (as counted by [`str::lines`]).
#[must_use]
pub fn line_count(content: &str) -> usize {
    content.lines().count()
}

/// The 1-based line of the chunk `content[bytes]` (whose first line is
/// `start_line`) that matches the most distinct `terms` (see [`QueryTerms`]);
/// the first such line on a tie, `start_line` when no line matches any.
///
/// The result is clamped to the lines of `content` (at least 1), so it can
/// always be passed to [`snippet`].
#[must_use]
pub fn best_line(
    content: &str,
    bytes: Range<usize>,
    start_line: usize,
    terms: &QueryTerms,
) -> usize {
    let chunk = clamped_slice(content, bytes);
    let mut best = (0usize, 0usize); // (matches, offset)
    for (offset, line) in chunk.lines().enumerate() {
        let matches = terms.count_in(line);
        if matches > best.0 {
            best = (matches, offset);
        }
    }
    let line = start_line.saturating_add(best.1);
    line.clamp(1, line_count(content).max(1))
}

/// One numbered line of a [`window`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NumberedLine<'a> {
    /// 1-based line number.
    pub number: usize,
    /// The line, without its terminator.
    pub text: &'a str,
}

/// The lines of `content` numbered `center - radius ..= center + radius`
/// (1-based) that exist. Empty when `center` is 0 or more than `radius` lines
/// past the end.
#[must_use]
pub fn window(content: &str, center: usize, radius: usize) -> Vec<NumberedLine<'_>> {
    if center == 0 {
        return Vec::new();
    }
    let first = center.saturating_sub(radius).max(1);
    let last = center.saturating_add(radius);
    content
        .lines()
        .enumerate()
        .skip(first - 1)
        .take(last - first + 1)
        .map(|(i, text)| NumberedLine {
            number: i + 1,
            text,
        })
        .collect()
}

/// Render `lines` as `"> 12345: text"` rows (`>` on line `marked`, a space on
/// the others), joined with newlines, without a trailing newline.
#[must_use]
pub fn render(lines: &[NumberedLine<'_>], marked: usize) -> String {
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let marker = if line.number == marked { '>' } else { ' ' };
        // Writing to a String cannot fail.
        let _ = write!(out, "{marker} {:>5}: {}", line.number, line.text);
    }
    out
}

/// `radius` lines on each side of `line`, rendered with [`render`]; `line`
/// is clamped to the lines of `content` first (an empty string for empty
/// content).
#[must_use]
pub fn snippet(content: &str, line: usize, radius: usize) -> String {
    let line = line.clamp(1, line_count(content).max(1));
    render(&window(content, line, radius), line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(q: &str) -> QueryTerms {
        QueryTerms::new(&[q], &[], 4)
    }

    #[test]
    fn tokens_split_identifiers_paths_and_prose() {
        assert_eq!(tokens("retryUpload(x)"), ["retry", "upload", "x"]);
        assert_eq!(
            tokens("HTTPServer utf8Name"),
            ["http", "server", "utf8", "name"]
        );
        assert_eq!(
            tokens("src/TheApp/max_retries.rs"),
            ["src", "the", "app", "max", "retries", "rs"]
        );
        assert_eq!(tokens("Où est-ce?"), ["où", "est", "ce"]);
        assert!(tokens("  -- ").is_empty());
    }

    #[test]
    fn query_terms_drop_stop_words_and_duplicates() {
        let stop = vec![
            "where".to_owned(),
            "The".to_owned(),
            "do".to_owned(),
            "we".to_owned(),
        ];
        let t = QueryTerms::new(&["Where do we retry", "the RETRY upload"], &stop, 4);
        assert_eq!(t.words(), ["retry", "upload"]);
        assert!(QueryTerms::new(&["where the"], &stop, 4).is_empty());
        assert_eq!(t.at_least(6).words(), ["upload"]);
    }

    #[test]
    fn query_terms_match_whole_tokens_or_long_prefixes() {
        let t = QueryTerms::new(&["the do embedding uploads"], &[], 4);
        assert_eq!(t.count_in("TheApp"), 1); // "the" is a token of it
        assert_eq!(t.count_in("documents other lowercase"), 0); // no substrings
        assert_eq!(t.count_in("fn embed(upload: &Upload)"), 2); // long prefixes
        let short = QueryTerms::new(&["re"], &[], 4);
        assert_eq!(short.count_in("retry"), 0); // too short for a prefix
    }

    #[test]
    fn best_line_ignores_stop_words_of_a_question() {
        let stop: Vec<String> = ["where", "do", "we", "the"]
            .iter()
            .map(|w| (*w).to_owned())
            .collect();
        let t = QueryTerms::new(&["where do we retry the failed uploads"], &stop, 4);
        let content = "// we do the other documents here
fn send() {}
for _ in 0..n { retry(upload) }
";
        assert_eq!(best_line(content, 0..content.len(), 1, &t), 3);
    }

    #[test]
    fn clamped_slice_never_splits_a_character() {
        let s = "aé€b"; // 1 + 2 + 3 + 1 bytes
        assert_eq!(clamped_slice(s, 0..2), "a"); // 2 is inside 'é'
        assert_eq!(clamped_slice(s, 2..7), "é€b"); // start 2 moves back to 1
        assert_eq!(clamped_slice(s, 4..5), ""); // both inside '€'
        assert_eq!(clamped_slice(s, 3..100), "€b");
        assert_eq!(clamped_slice(s, 100..200), "");
        #[allow(clippy::reversed_empty_ranges)] // a reversed range is a valid input
        let reversed = 5..2;
        assert_eq!(clamped_slice(s, reversed), "");
    }

    #[test]
    fn best_line_picks_the_line_with_most_words() {
        let content = "zero\none foo\ntwo foo bar\nthree\n";
        let all = 0..content.len();
        assert_eq!(best_line(content, all.clone(), 1, &words("foo bar")), 3);
        assert_eq!(best_line(content, all, 1, &words("nothing")), 1);
        // A chunk starting at line 2 (byte 5).
        assert_eq!(best_line(content, 5..content.len(), 2, &words("bar")), 3);
    }

    #[test]
    fn best_line_survives_a_shrunk_file() {
        // Offsets and lines from an index entry of a much longer file.
        let content = "only\ntwo lines\n";
        assert_eq!(best_line(content, 500..900, 9988, &words("x")), 2);
        assert_eq!(best_line("", 500..900, 9988, &words("x")), 1);
    }

    #[test]
    fn best_line_survives_offsets_inside_characters() {
        let content = "é\n€ needle\n";
        for start in 0..content.len() + 2 {
            for end in 0..content.len() + 2 {
                let line = best_line(content, start..end, 1, &words("needle"));
                assert!((1..=2).contains(&line));
            }
        }
    }

    #[test]
    fn window_clips_at_both_ends() {
        let content = "a\nb\nc\nd\n";
        let nums =
            |c, r| -> Vec<usize> { window(content, c, r).iter().map(|l| l.number).collect() };
        assert_eq!(nums(1, 2), [1, 2, 3]);
        assert_eq!(nums(4, 1), [3, 4]);
        assert_eq!(nums(6, 2), [4]);
        assert!(nums(9988, 10).is_empty()); // the old `vec context` panic
        assert!(nums(0, 3).is_empty());
        assert_eq!(nums(2, usize::MAX), [1, 2, 3, 4]);
    }

    #[test]
    fn render_marks_one_line() {
        let content = "a\nb\nc";
        assert_eq!(
            render(&window(content, 2, 1), 2),
            "      1: a\n>     2: b\n      3: c"
        );
    }

    #[test]
    fn snippet_clamps_a_line_past_the_end() {
        assert_eq!(snippet("a\nb", 50, 0), ">     2: b");
        assert_eq!(snippet("", 3, 2), "");
    }
}
