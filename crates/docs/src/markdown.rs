//! Markdown helpers shared by the parsers: tables, byte sizes, HTML → Markdown,
//! code fences and the fence-aware heading scan used by `doc.outline`.

use regex::Regex;
use std::sync::LazyLock;

/// `_rows_to_markdown`: row 0 is the header; pipes are escaped so a cell
/// cannot fabricate columns; short rows are padded.
pub(crate) fn rows_to_markdown(rows: &[Vec<String>], max_cols: usize) -> String {
    let Some(width) = rows.iter().map(Vec::len).max() else {
        return String::new();
    };
    let width = width.min(max_cols);
    if width == 0 {
        return String::new();
    }
    let line = |cells: &[String]| {
        let mut out = String::from("| ");
        for i in 0..width {
            if i > 0 {
                out.push_str(" | ");
            }
            if let Some(c) = cells.get(i) {
                out.push_str(&c.replace('|', "\\|"));
            }
        }
        out.push_str(" |");
        out
    };
    let mut out = vec![
        line(&rows[0]),
        format!("|{}|", vec![" --- "; width].join("|")),
    ];
    out.extend(rows[1..].iter().map(|r| line(r)));
    out.join("\n")
}

/// `_human_bytes`: `"512 B"`, `"1.5 KB"`, ... capped at GB.
pub(crate) fn human_bytes(n: u64) -> String {
    let mut value = n as f64;
    for unit in ["B", "KB", "MB", "GB"] {
        if value < 1024.0 || unit == "GB" {
            return if unit == "B" {
                format!("{value:.0} B")
            } else {
                format!("{value:.1} {unit}")
            };
        }
        value /= 1024.0;
    }
    format!("{value:.1} GB")
}

/// `_parse_code`: fence with the language, using a fence longer than any
/// backtick run inside the text so the file cannot break out of it.
pub(crate) fn fence_code(text: &str, lang: &str) -> String {
    let (mut longest, mut run) = (0usize, 0usize);
    for ch in text.chars() {
        run = if ch == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat(3.max(longest + 1));
    format!("{fence}{lang}\n{text}\n{fence}")
}

static TITLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<title[^>]*>(.*?)</title>").expect("regex"));
static TAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]*>").expect("regex"));

/// The `<title>` of an HTML document (first 64 KiB), whitespace-collapsed.
pub(crate) fn html_title(html: &str) -> Option<String> {
    let head = &html[..floor_char_boundary(html, 64 * 1024)];
    let raw = TITLE.captures(head)?.get(1)?.as_str();
    let text = decode_entities(&TAGS.replace_all(raw, " "));
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!t.is_empty()).then_some(t)
}

fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&#X27;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

/// Largest index <= `i` that is a char boundary of `s`.
pub(crate) fn floor_char_boundary(s: &str, i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    let mut i = i;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Maximum HTML byte length fed to `htmd` (prevents unbounded DOM memory/stack).
const MAX_HTML_INPUT_BYTES: usize = 5 * 1024 * 1024;
/// Maximum HTML tag nesting depth fed to `htmd` (prevents recursive stack overflow / SIGABRT).
const MAX_HTML_NESTING_DEPTH: usize = 512;

const SKIP_BLOCK_TAGS: [&[u8]; 8] = [
    b"head",
    b"script",
    b"style",
    b"noscript",
    b"template",
    b"iframe",
    b"object",
    b"embed",
];

fn is_void_html_tag(tag_lower: &[u8]) -> bool {
    matches!(
        tag_lower,
        b"area"
            | b"base"
            | b"br"
            | b"col"
            | b"embed"
            | b"hr"
            | b"img"
            | b"input"
            | b"link"
            | b"meta"
            | b"param"
            | b"source"
            | b"track"
            | b"wbr"
    )
}

/// Linear, zero-allocation ASCII case-insensitive search for `needle_lower` in `haystack`.
fn find_ascii_ci(haystack: &[u8], needle_lower: &[u8]) -> Option<usize> {
    if needle_lower.is_empty() || haystack.len() < needle_lower.len() {
        return None;
    }
    haystack
        .windows(needle_lower.len())
        .position(|w| w.eq_ignore_ascii_case(needle_lower))
}

/// O(n) byte-level tag nesting depth probe; returns `true` if open-tag depth
/// exceeds `max_depth` (protecting `htmd` from stack overflow on hostile HTML).
fn html_nesting_exceeds(html: &str, max_depth: usize) -> bool {
    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut depth = 0usize;

    while i < len {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        // HTML comment: <!-- ... -->
        if bytes[i..].starts_with(b"<!--") {
            match find_ascii_ci(&bytes[i + 4..], b"-->") {
                Some(rel) => i = i + 4 + rel + 3,
                None => break,
            }
            continue;
        }
        let Some(first) = bytes.get(i + 1).copied() else {
            break;
        };
        // Declaration or PI: <!...> or <?...>
        if first == b'!' || first == b'?' {
            match bytes[i + 2..].iter().position(|&b| b == b'>') {
                Some(rel) => i = i + 2 + rel + 1,
                None => break,
            }
            continue;
        }
        let is_close = first == b'/';
        let name_start = if is_close { i + 2 } else { i + 1 };
        if name_start >= len || !bytes[name_start].is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let mut name_end = name_start;
        while name_end < len
            && (bytes[name_end].is_ascii_alphanumeric()
                || matches!(bytes[name_end], b'-' | b'_' | b':'))
        {
            name_end += 1;
        }
        let mut tag_lower = [0u8; 16];
        let tag_len = (name_end - name_start).min(tag_lower.len());
        for (dst, &src) in tag_lower[..tag_len]
            .iter_mut()
            .zip(&bytes[name_start..name_start + tag_len])
        {
            *dst = src.to_ascii_lowercase();
        }
        let tag_slice = &tag_lower[..tag_len];

        // Advance to closing `>`, respecting quoted attribute values.
        let mut j = name_end;
        let mut quote: Option<u8> = None;
        while j < len {
            let b = bytes[j];
            if let Some(q) = quote {
                if b == q {
                    quote = None;
                }
            } else if b == b'"' || b == b'\'' {
                quote = Some(b);
            } else if b == b'>' {
                break;
            }
            j += 1;
        }
        if j >= len {
            break;
        }
        let self_closing = !is_close && j > name_start && bytes[j - 1] == b'/';
        i = j + 1;

        if is_close {
            depth = depth.saturating_sub(1);
            continue;
        }
        if !self_closing && (tag_slice == b"script" || tag_slice == b"style") {
            let needle: &[u8] = if tag_slice == b"script" {
                b"</script"
            } else {
                b"</style"
            };
            match find_ascii_ci(&bytes[i..], needle) {
                Some(rel) => {
                    let after = i + rel + needle.len();
                    match bytes[after..].iter().position(|&b| b == b'>') {
                        Some(gt) => i = after + gt + 1,
                        None => break,
                    }
                }
                None => break,
            }
            continue;
        }
        if !self_closing && !is_void_html_tag(tag_slice) {
            depth += 1;
            if depth > max_depth {
                return true;
            }
        }
    }
    false
}

/// Linear, non-recursive HTML tag stripper used when input exceeds size/nesting
/// caps or `htmd` fails. Drops `<head>`/`<script>`/`<style>`/etc. blocks cleanly.
fn strip_html_tags_flat(html: &str) -> String {
    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut raw = String::with_capacity(len.min(64 * 1024));
    let mut i = 0usize;

    while i < len {
        if bytes[i] != b'<' {
            let start = i;
            while i < len && bytes[i] != b'<' {
                i += 1;
            }
            raw.push_str(&html[start..i]);
            continue;
        }
        if bytes[i..].starts_with(b"<!--") {
            match find_ascii_ci(&bytes[i + 4..], b"-->") {
                Some(rel) => i = i + 4 + rel + 3,
                None => break,
            }
            raw.push(' ');
            continue;
        }
        let Some(first) = bytes.get(i + 1).copied() else {
            break;
        };
        if first == b'!' || first == b'?' {
            match bytes[i + 2..].iter().position(|&b| b == b'>') {
                Some(rel) => i = i + 2 + rel + 1,
                None => break,
            }
            raw.push(' ');
            continue;
        }
        let is_close = first == b'/';
        let name_start = if is_close { i + 2 } else { i + 1 };
        if name_start >= len || !bytes[name_start].is_ascii_alphabetic() {
            raw.push('<');
            i += 1;
            continue;
        }
        let mut name_end = name_start;
        while name_end < len
            && (bytes[name_end].is_ascii_alphanumeric()
                || matches!(bytes[name_end], b'-' | b'_' | b':'))
        {
            name_end += 1;
        }
        let mut tag_lower = [0u8; 16];
        let tag_len = (name_end - name_start).min(tag_lower.len());
        for (dst, &src) in tag_lower[..tag_len]
            .iter_mut()
            .zip(&bytes[name_start..name_start + tag_len])
        {
            *dst = src.to_ascii_lowercase();
        }
        let tag_slice = &tag_lower[..tag_len];

        let mut j = name_end;
        let mut quote: Option<u8> = None;
        while j < len {
            let b = bytes[j];
            if let Some(q) = quote {
                if b == q {
                    quote = None;
                }
            } else if b == b'"' || b == b'\'' {
                quote = Some(b);
            } else if b == b'>' {
                break;
            }
            j += 1;
        }
        if j >= len {
            break;
        }
        let self_closing = !is_close && j > name_start && bytes[j - 1] == b'/';
        i = j + 1;
        raw.push(' ');

        if !is_close && !self_closing && SKIP_BLOCK_TAGS.contains(&tag_slice) {
            let mut close_needle = [0u8; 18];
            close_needle[0] = b'<';
            close_needle[1] = b'/';
            close_needle[2..2 + tag_len].copy_from_slice(tag_slice);
            let needle = &close_needle[..2 + tag_len];
            match find_ascii_ci(&bytes[i..], needle) {
                Some(rel) => {
                    let after = i + rel + needle.len();
                    match bytes[after..].iter().position(|&b| b == b'>') {
                        Some(gt) => i = after + gt + 1,
                        None => break,
                    }
                }
                None => break,
            }
        }
    }

    decode_entities(&raw)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// HTML → Markdown (`markdownify(heading_style="ATX", bullets="-")`), via
/// `htmd`. Scripts, styles and the `<head>` are dropped; the result is trimmed.
/// Hostile inputs exceeding [`MAX_HTML_INPUT_BYTES`] or [`MAX_HTML_NESTING_DEPTH`]
/// degrade to non-recursive tag stripping so deeply nested markup can never
/// overflow the stack.
pub(crate) fn html_to_markdown(html: &str) -> String {
    if html.len() > MAX_HTML_INPUT_BYTES || html_nesting_exceeds(html, MAX_HTML_NESTING_DEPTH) {
        let capped = &html[..floor_char_boundary(html, MAX_HTML_INPUT_BYTES)];
        return strip_html_tags_flat(capped);
    }
    let converter = htmd::HtmlToMarkdown::builder()
        .options(htmd::options::Options {
            heading_style: htmd::options::HeadingStyle::Atx,
            bullet_list_marker: htmd::options::BulletListMarker::Dash,
            ul_bullet_spacing: 1,
            ol_number_spacing: 1,
            ..htmd::options::Options::default()
        })
        .skip_tags(vec![
            "head", "script", "style", "noscript", "template", "iframe", "object", "embed",
        ])
        .build();
    match converter.convert(html) {
        Ok(md) => md.trim().to_owned(),
        // htmd only fails on I/O of its sink; degrade to non-recursive tag stripping.
        Err(_) => strip_html_tags_flat(html),
    }
}

/// One ATX heading found by [`heading_sections`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Heading {
    pub title: String,
    pub level: u8,
    pub start_char: usize,
}

/// ATX headings (`#`..`######` + space) outside fenced code blocks, with
/// their char offset in `text` (so `doc.read(start = startChar)` lands on
/// the heading line). At most `limit` headings.
pub(crate) fn heading_sections(text: &str, limit: usize) -> Vec<Heading> {
    let mut out = Vec::new();
    let mut offset = 0usize; // chars before the current line
    let mut fence: Option<(char, usize)> = None;
    for line in text.split_inclusive('\n') {
        let line_chars = line.chars().count();
        let body = line.trim_end_matches(['\n', '\r']);
        let indent = body.len() - body.trim_start_matches(' ').len();
        let trimmed = &body[indent..];
        if indent <= 3 {
            if let Some((ch, n)) = fence_open(trimmed) {
                match fence {
                    None => fence = Some((ch, n)),
                    Some((fc, fnum))
                        if fc == ch
                            && n >= fnum
                            && trimmed.trim_start_matches(ch).trim().is_empty() =>
                    {
                        fence = None;
                    }
                    Some(_) => {}
                }
            } else if fence.is_none()
                && let Some(h) = atx_heading(trimmed)
            {
                out.push(Heading {
                    title: h.1,
                    level: h.0,
                    start_char: offset,
                });
                if out.len() >= limit {
                    break;
                }
            }
        }
        offset += line_chars;
    }
    out
}

fn fence_open(s: &str) -> Option<(char, usize)> {
    let ch = s.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let n = s.chars().take_while(|c| *c == ch).count();
    (n >= 3).then_some((ch, n))
}

fn atx_heading(s: &str) -> Option<(u8, String)> {
    let hashes = s.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &s[hashes..];
    if !(rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')) {
        return None;
    }
    let mut title = rest.trim();
    // Optional closing sequence: " ##".
    let stripped = title.trim_end_matches('#');
    if stripped.len() != title.len() && (stripped.is_empty() || stripped.ends_with(' ')) {
        title = stripped.trim_end();
    }
    if title.is_empty() {
        return None;
    }
    Some((hashes as u8, title.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(v: &[&[&str]]) -> Vec<Vec<String>> {
        v.iter()
            .map(|r| r.iter().map(|c| (*c).to_owned()).collect())
            .collect()
    }

    #[test]
    fn tables_pad_escape_and_cap_columns() {
        assert_eq!(
            rows_to_markdown(&rows(&[&["name", "qty"], &["gad|get", "4"]]), 30),
            "| name | qty |\n| --- | --- |\n| gad\\|get | 4 |"
        );
        assert_eq!(
            rows_to_markdown(&rows(&[&["a", "b", "c"], &["1"]]), 30),
            "| a | b | c |\n| --- | --- | --- |\n| 1 |  |  |"
        );
        assert_eq!(
            rows_to_markdown(&rows(&[&["a", "b", "c"]]), 2),
            "| a | b |\n| --- | --- |"
        );
        assert_eq!(rows_to_markdown(&[], 30), "");
        assert_eq!(rows_to_markdown(&[vec![]], 30), "");
    }

    #[test]
    fn human_bytes_matches_python() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024 * 1024), "3072.0 GB");
    }

    #[test]
    fn test_code_is_fenced_with_its_language() {
        assert!(fence_code("print(1)\n", "python").starts_with("```python\n"));
    }

    #[test]
    fn test_code_fence_widens_past_backticks_in_the_file() {
        let out = fence_code("a = '''```'''\n", "python");
        assert!(out.starts_with("````python"));
        assert!(out.trim_end().ends_with("````"));
    }

    #[test]
    fn html_converts_to_atx_markdown_with_dash_bullets() {
        let md = html_to_markdown(
            "<html><head><title>T</title><style>p{}</style></head><body><h1>Head</h1>\
             <p>Para <b>bold</b></p><ul><li>one</li><li>two</li></ul><script>evil()</script></body></html>",
        );
        assert!(md.starts_with("# Head"), "{md}");
        assert!(md.contains("- one") && md.contains("**bold**"), "{md}");
        assert!(!md.contains("evil") && !md.contains("p{}"), "{md}");
        assert_eq!(
            html_title("<title>  A &amp; B\n</title>"),
            Some("A & B".to_owned())
        );
        assert_eq!(
            html_title("<title>&amp;lt;tag&amp;gt; &apos;ok&#x27;</title>"),
            Some("&lt;tag&gt; 'ok'".to_owned())
        );
        assert_eq!(html_title("<p>none</p>"), None);
    }

    #[test]
    fn heading_scan_skips_fenced_code_and_reports_char_offsets() {
        let text = "intro é\n# One\n\n```\n# not a heading\n```\n## Two ##\n#nospace\n####### seven\n### Three";
        let hs = heading_sections(text, 100);
        let got: Vec<(&str, u8)> = hs.iter().map(|h| (h.title.as_str(), h.level)).collect();
        assert_eq!(got, [("One", 1), ("Two", 2), ("Three", 3)]);
        for h in &hs {
            let rest: String = text.chars().skip(h.start_char).collect();
            assert!(rest.starts_with('#'), "{rest:?}");
        }
        assert_eq!(hs[0].start_char, "intro é\n".chars().count());
        assert_eq!(heading_sections(text, 2).len(), 2);
    }

    #[test]
    fn deeply_nested_html_falls_back_without_stack_overflow() {
        let mut hostile = "<div>".repeat(20_000);
        hostile.push_str("<script>evil()</script>deep text &amp; safe");
        hostile.push_str(&"</div>".repeat(20_000));
        let out = html_to_markdown(&hostile);
        assert_eq!(out, "deep text & safe");
    }

    #[test]
    fn many_script_tags_scan_in_linear_time() {
        let mut html = "<script>alert(1)</script>".repeat(20_000);
        html.push_str("<p>visible</p>");
        let out = html_to_markdown(&html);
        assert_eq!(out, "visible");
    }
}
