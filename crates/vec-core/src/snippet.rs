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

/// The lowercased words of `queries`, in order, used by [`best_line`].
#[must_use]
pub fn query_words<S: AsRef<str>>(queries: &[S]) -> Vec<String> {
    queries
        .iter()
        .flat_map(|q| q.as_ref().split_whitespace())
        .map(str::to_lowercase)
        .collect()
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
/// `start_line`) that contains the most distinct `words` (lowercase); the
/// first such line on a tie, `start_line` when no line contains any word.
///
/// The result is clamped to the lines of `content` (at least 1), so it can
/// always be passed to [`snippet`].
#[must_use]
pub fn best_line(content: &str, bytes: Range<usize>, start_line: usize, words: &[String]) -> usize {
    let chunk = clamped_slice(content, bytes);
    let mut best = (0usize, 0usize); // (matches, offset)
    for (offset, line) in chunk.lines().enumerate() {
        let lower = line.to_lowercase();
        let matches = words.iter().filter(|w| lower.contains(w.as_str())).count();
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

    fn words(q: &str) -> Vec<String> {
        query_words(&[q])
    }

    #[test]
    fn query_words_lowercases_every_query() {
        assert_eq!(query_words(&["Foo bar", "BAZ"]), ["foo", "bar", "baz"]);
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
