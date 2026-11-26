//! Pagination (port of `documents._slice` / `_source_chars_consumed`).
//!
//! `returned_chars` counts SOURCE characters consumed, never the length of
//! `content`: [`fsm_core::text::smart_truncate`] may append a marker that is
//! not in the source, so paginating with `start + returned_chars` must land
//! on the first unread source character (no gap, no overlap). The consumed
//! count mirrors core's `smart_truncate` — the function that actually
//! produces the content — so the invariant
//! `content - marker == source[start .. start + returned_chars]` holds exactly.

use fsm_core::text::{HARD_MARKER, clip_chars, smart_truncate};

/// Same boundary set, same order as core's (private) `BOUNDARIES`.
const BOUNDARIES: [&str; 8] = ["\n\n", "\n", "。", ". ", "！", "! ", "？", "? "];

/// One page of a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sliced {
    pub content: String,
    /// True only when text was withheld: a soft cut, or the slice ended before EOF.
    pub truncated: bool,
    /// Source characters consumed (the pagination step).
    pub returned_chars: usize,
    /// `start` clamped into `[0, total]`.
    pub start: usize,
}

/// Byte offset of char index `idx` (clamped to the end).
fn byte_at(s: &str, idx: usize) -> usize {
    s.char_indices().nth(idx).map_or(s.len(), |(b, _)| b)
}

/// `_slice(full, start, length)` with `max_content_chars` passed in.
/// `total_chars` is `full.chars().count()` (precomputed by the caller).
pub(crate) fn slice(
    full: &str,
    total_chars: usize,
    start: usize,
    length: Option<usize>,
    max_content_chars: usize,
) -> Sliced {
    let start = start.min(total_chars);
    let end = match length {
        None => total_chars,
        Some(len) => total_chars.min(start.saturating_add(len)),
    };
    let b0 = byte_at(full, start);
    let b1 = b0 + byte_at(&full[b0..], end - start);
    let chunk = &full[b0..b1];

    let (mut content, soft) = smart_truncate(chunk, max_content_chars);
    let (returned_chars, soft_end) = if soft {
        let consumed = source_chars_consumed(chunk, max_content_chars);
        let kept = clip_chars(chunk, consumed);
        if consumed > 0 && kept.trim().is_empty() {
            // An all-whitespace prefix is shown rather than dropped, so a
            // pagination client always sees progress.
            content = format!("{kept}{HARD_MARKER}");
        }
        (consumed, start + consumed)
    } else {
        (end - start, end)
    };
    Sliced {
        content,
        truncated: soft || soft_end < total_chars,
        returned_chars,
        start,
    }
}

/// How many leading SOURCE chars `smart_truncate(chunk, max_chars)` kept
/// (marker excluded). Never 0 for a non-empty chunk, so pagination advances.
pub(crate) fn source_chars_consumed(chunk: &str, max_chars: usize) -> usize {
    let n = chunk.chars().count();
    if n <= max_chars {
        return n;
    }
    let head = clip_chars(chunk, max_chars);
    let floor = max_chars * 7 / 10;
    let mut best: Option<usize> = None; // byte end of the kept part
    let mut best_chars = 0usize;
    for sep in BOUNDARIES {
        if let Some(idx) = head.rfind(sep) {
            let idx_chars = head[..idx].chars().count();
            let end_chars = idx_chars + sep.chars().count();
            if idx_chars >= floor && end_chars > best_chars {
                best_chars = end_chars;
                best = Some(idx + sep.len());
            }
        }
    }
    match best {
        Some(end) if best_chars > 0 => match head[..end].trim_end().chars().count() {
            0 => best_chars,
            k => k,
        },
        _ => match head.trim_end().chars().count() {
            0 => max_chars,
            k => k,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsm_core::text::BOUNDARY_MARKER;

    fn strip_marker(s: &str) -> &str {
        for m in [BOUNDARY_MARKER, HARD_MARKER] {
            if let Some(x) = s.strip_suffix(m) {
                return x;
            }
        }
        s
    }

    fn chars(s: &str, a: usize, b: usize) -> String {
        s.chars().skip(a).take(b - a).collect()
    }

    #[test]
    fn test_pagination_slices_offset() {
        let full = format!("{}{}", "A".repeat(1000), "B".repeat(1000));
        let head = slice(&full, 2000, 0, Some(500), 50_000);
        assert_eq!(
            (head.returned_chars, head.start, head.truncated),
            (500, 0, true)
        );
        assert!(head.content.starts_with("AAAAA"));
        let mid = slice(&full, 2000, 900, Some(200), 50_000);
        assert_eq!((mid.start, mid.returned_chars), (900, 200));
        assert!(mid.content.starts_with('A') && mid.content.contains('B'));
    }

    #[test]
    fn test_pagination_zero_length_returns_to_end() {
        let out = slice("hi there", 8, 0, None, 50_000);
        assert_eq!(
            (out.start, out.returned_chars, out.truncated),
            (0, 8, false)
        );
        assert_eq!(out.content, "hi there");
    }

    #[test]
    fn test_slice_start_past_eof_reports_clamped_start() {
        let out = slice("0123456789", 10, 999_999, None, 50_000);
        assert_eq!(out.start, 10);
        assert_eq!(out.returned_chars, 0);
        assert_eq!(out.content, "");
        assert!(!out.truncated);
        let huge = slice("0123456789", 10, 3, Some(usize::MAX), 50_000);
        assert_eq!((huge.returned_chars, huge.truncated), (7, false));
    }

    #[test]
    fn test_slice_returned_chars_is_source_consumed() {
        let body = format!(
            "{}\n\n{}",
            "alpha beta gamma delta. ".repeat(4),
            "more text here. ".repeat(40)
        );
        let total = body.chars().count();
        let first = slice(&body, total, 0, None, 120);
        assert!(first.truncated);
        let displayed = strip_marker(&first.content);
        assert_eq!(
            displayed,
            chars(&body, 0, first.returned_chars),
            "marker leaked"
        );
        let second = slice(&body, total, first.returned_chars, None, 120);
        assert!(second.content.starts_with(&chars(
            &body,
            first.returned_chars,
            first.returned_chars + 20
        )));
    }

    #[test]
    fn test_whitespace_pagination_always_advances() {
        let text = format!("{}actual content", " ".repeat(200));
        let total = text.chars().count();
        let mut offset = 0;
        let mut done = false;
        for _ in 0..4 {
            let s = slice(&text, total, offset, None, 120);
            if !s.truncated {
                assert!(s.content.contains("actual content"));
                done = true;
                break;
            }
            assert!(s.returned_chars > 0);
            offset += s.returned_chars;
        }
        assert!(done, "pagination never reached EOF");
    }

    /// Walk every page of many generated documents: pages must tile the
    /// source exactly, each page's content (minus marker) must be the
    /// source run it consumed, and the walk must terminate.
    #[test]
    fn walking_pages_reconstructs_the_source_exactly() {
        let pieces = [
            "word ",
            "句子。",
            "end. ",
            "\n",
            "\n\n",
            "  ",
            "é",
            "ä½ å¥½",
            "! ",
            "？",
            "\t",
            "x",
            "長い文章の一部",
            "   \n   ",
            "🙂",
        ];
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for case in 0..300 {
            let len = (next() % 400) as usize;
            let mut doc = String::new();
            for _ in 0..len {
                doc.push_str(pieces[(next() % pieces.len() as u64) as usize]);
            }
            let total = doc.chars().count();
            let max = 5 + (next() % 90) as usize;
            let length = if case % 3 == 0 {
                Some(1 + (next() % 150) as usize)
            } else {
                None
            };
            let mut rebuilt = String::new();
            let mut start = 0;
            let mut steps = 0;
            loop {
                let s = slice(&doc, total, start, length, max);
                assert_eq!(s.start, start.min(total));
                let shown = strip_marker(&s.content);
                let consumed = chars(&doc, start, start + s.returned_chars);
                if shown != consumed {
                    // Only the all-whitespace special case may differ, and
                    // then the shown text is exactly the consumed run.
                    panic!("case {case}: shown {shown:?} != consumed {consumed:?} (max {max})");
                }
                rebuilt.push_str(&consumed);
                start += s.returned_chars;
                steps += 1;
                if start >= total {
                    assert!(!s.truncated || s.returned_chars == 0 || start >= total);
                    break;
                }
                assert!(s.truncated, "case {case}: not at EOF but not truncated");
                assert!(s.returned_chars > 0, "case {case}: no progress at {start}");
                assert!(steps <= total + 1, "case {case}: walk does not terminate");
            }
            assert_eq!(rebuilt, doc, "case {case}");
        }
    }

    #[test]
    fn consumed_matches_core_truncation_for_hard_and_soft_cuts() {
        // Soft cut on a paragraph boundary.
        let s = format!("{}\n\n{}", "a".repeat(80), "b".repeat(100));
        assert_eq!(source_chars_consumed(&s, 100), 80);
        // Hard cut, trailing spaces trimmed by core.
        let h = format!("{}   {}", "c".repeat(97), "d".repeat(50));
        assert_eq!(source_chars_consumed(&h, 100), 97);
        // Pure whitespace head: never zero.
        assert_eq!(source_chars_consumed(&" ".repeat(300), 120), 120);
        assert_eq!(source_chars_consumed("short", 100), 5);
    }
}
