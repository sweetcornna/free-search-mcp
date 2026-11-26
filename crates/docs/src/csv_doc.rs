//! CSV/TSV → Markdown table (port of `_parse_csv`).
//!
//! Parsing uses the `csv` crate. `csv-core` silently skips empty lines while
//! Python's `csv.reader` yields them as empty rows, and the oracle counts
//! them against the row and render budgets ("blank rows consume resources
//! too"). The reader's byte position before and after each record brackets
//! the skipped blank lines plus the record itself, so counting line breaks
//! in that span (minus the record's own) recovers them exactly.

use crate::decode::decode_body;
use crate::markdown::rows_to_markdown;
use crate::parsed::cap_chars;

/// Most rows rendered from a CSV or across a workbook.
pub(crate) const MAX_TABLE_ROWS: usize = 10_000;
/// Most columns rendered per row.
pub(crate) const MAX_TABLE_COLS: usize = 30;

/// Delimiter guess over the first 8 KiB (`csv.Sniffer().sniff(sample,
/// delimiters=",;\t|")`, simplified): the candidate whose per-line count
/// (outside double quotes) is the most consistent non-zero value wins; ties
/// go to `,` then tab, `;`, `|`. No consistent candidate → `,` (excel).
pub(crate) fn sniff_delimiter(sample: &str) -> u8 {
    let lines: Vec<&str> = sample
        .split('\n')
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.is_empty())
        .take(50)
        .collect();
    if lines.is_empty() {
        return b',';
    }
    let mut best: Option<(u8, f64)> = None;
    for delim in *b",\t;|" {
        let counts: Vec<usize> = lines.iter().map(|l| count_unquoted(l, delim)).collect();
        let mut freq: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
        for c in &counts {
            *freq.entry(*c).or_default() += 1;
        }
        let Some((&mode, &n)) = freq.iter().max_by_key(|(c, n)| (**n, **c)) else {
            continue;
        };
        if mode == 0 {
            continue;
        }
        let consistency = n as f64 / counts.len() as f64;
        if consistency < 0.9 {
            continue;
        }
        if best.is_none_or(|(_, c)| consistency > c) {
            best = Some((delim, consistency));
        }
    }
    best.map_or(b',', |(d, _)| d)
}

/// Line breaks (`\r\n`, `\n` or a lone `\r`) in `b`.
fn count_breaks(b: &[u8]) -> usize {
    let (mut n, mut i) = (0, 0);
    while i < b.len() {
        match b[i] {
            b'\n' => n += 1,
            b'\r' => {
                n += 1;
                if b.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    n
}

fn count_unquoted(line: &str, delim: u8) -> usize {
    let mut in_quotes = false;
    let mut n = 0;
    for b in line.bytes() {
        if b == b'"' {
            in_quotes = !in_quotes;
        } else if b == delim && !in_quotes {
            n += 1;
        }
    }
    n
}

/// `_parse_csv(blob, ctype)` → `(markdown, truncated, (rows, cols))`.
/// `rows`/`cols` describe the source table as far as it was read.
pub(crate) fn parse_csv(
    blob: &[u8],
    ctype: &str,
    max_chars: usize,
) -> (String, bool, (usize, usize)) {
    let text = decode_body(blob, ctype);
    let sample_end = text.char_indices().nth(8192).map_or(text.len(), |(b, _)| b);
    let delim = sniff_delimiter(&text[..sample_end]);

    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(delim)
        .from_reader(text.as_bytes());
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut acc = 0usize;
    let mut truncated = false;
    let mut index = 0usize;
    let mut max_cols = 0usize;
    let raw = text.as_bytes();
    let mut record = csv::ByteRecord::new();
    // csv-core ends a CRLF record at the \r; its \n then leads the next span.
    let span_start = |before: usize| {
        if before > 0 && raw[before - 1] == b'\r' && raw.get(before) == Some(&b'\n') {
            before + 1
        } else {
            before
        }
    };

    // Returns false once a budget stops the read.
    let add_row = |cells: Vec<String>,
                   raw_len: usize,
                   rows: &mut Vec<Vec<String>>,
                   acc: &mut usize,
                   truncated: &mut bool,
                   index: &mut usize|
     -> bool {
        if *index >= MAX_TABLE_ROWS {
            *truncated = true;
            return false;
        }
        *index += 1;
        *truncated |= raw_len > MAX_TABLE_COLS;
        *acc += cells
            .iter()
            .map(|c| c.replace('|', "\\|").chars().count())
            .sum::<usize>()
            + 3 * cells.len()
            + 4;
        rows.push(cells);
        if *acc >= max_chars {
            *truncated = true;
            return false;
        }
        true
    };

    'read: loop {
        let before = usize::try_from(reader.position().byte())
            .unwrap_or(raw.len())
            .min(raw.len());
        match reader.read_byte_record(&mut record) {
            Ok(true) => {
                let after = usize::try_from(reader.position().byte())
                    .unwrap_or(raw.len())
                    .min(raw.len());
                let span = &raw[span_start(before).min(after.max(before))..after.max(before)];
                let own: usize = record.iter().map(count_breaks).sum::<usize>()
                    + usize::from(matches!(span.last(), Some(b'\n' | b'\r')));
                for _ in 0..count_breaks(span).saturating_sub(own) {
                    if !add_row(
                        Vec::new(),
                        0,
                        &mut rows,
                        &mut acc,
                        &mut truncated,
                        &mut index,
                    ) {
                        break 'read;
                    }
                }
                max_cols = max_cols.max(record.len());
                let cells: Vec<String> = record
                    .iter()
                    .take(MAX_TABLE_COLS)
                    .map(|c| String::from_utf8_lossy(c).trim().to_owned())
                    .collect();
                if !add_row(
                    cells,
                    record.len(),
                    &mut rows,
                    &mut acc,
                    &mut truncated,
                    &mut index,
                ) {
                    break;
                }
            }
            Ok(false) => {
                for _ in 0..count_breaks(&raw[span_start(before).min(raw.len())..]) {
                    if !add_row(
                        Vec::new(),
                        0,
                        &mut rows,
                        &mut acc,
                        &mut truncated,
                        &mut index,
                    ) {
                        break;
                    }
                }
                break;
            }
            // Python's csv module is lenient too: keep what parsed.
            Err(_) => break,
        }
    }
    let n_rows = rows.len();
    let output = rows_to_markdown(&rows, MAX_TABLE_COLS);
    let chars = output.chars().count();
    let (out, _, cut) = cap_chars(output, chars, max_chars);
    (out, truncated || cut, (n_rows, max_cols))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn csv(blob: &[u8]) -> (String, bool) {
        let (t, tr, _) = parse_csv(blob, "", 2_000_000);
        (t, tr)
    }

    #[test]
    fn test_csv_becomes_a_markdown_table() {
        let (text, truncated) = csv(b"name,qty\nwidget,3\n");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "| name | qty |");
        assert_eq!(lines[1], "| --- | --- |");
        assert!(!truncated);
    }

    #[test]
    fn test_csv_escapes_pipes_so_cells_cannot_forge_columns() {
        assert!(csv(b"name,qty\ngad|get,4\n").0.contains(r"gad\|get"));
    }

    #[test]
    fn test_csv_detects_tab_delimiters() {
        assert!(csv(b"name\tqty\nwidget\t3\n").0.contains("| name | qty |"));
    }

    #[test]
    fn test_csv_ragged_rows_are_padded() {
        assert!(csv(b"a,b,c\n1\n").0.contains("| 1 |  |  |"));
    }

    #[test]
    fn test_csv_is_capped_by_max_document_chars() {
        let mut body = b"a,b\n".to_vec();
        for _ in 0..500 {
            body.extend_from_slice(b"xxxx,yyyy\n");
        }
        assert!(parse_csv(&body, "", 20).1);
    }

    #[test]
    fn test_read_doc_csv_parser_honours_header_charset() {
        let (gbk, _, _) = encoding_rs::GBK.encode("列一,列二\n中文,值");
        let (table, _, _) = parse_csv(&gbk, "text/csv; charset=gbk", 2_000_000);
        assert!(table.contains("列一") && table.contains("中文"), "{table}");
    }

    #[test]
    fn test_empty_csv_rows_cannot_evade_document_budget() {
        let mut body = b"a,b\n".to_vec();
        body.extend(b",\n".repeat(20_000));
        let (text, truncated, _) = parse_csv(&body, "", 100);
        assert!(truncated);
        assert!(text.chars().count() <= 100);
    }

    #[test]
    fn test_csv_blank_and_escaped_rows_have_bounded_rendering() {
        for row in [&b"\n"[..], b",\n", b"   ,   \n", b"\"|||\",\n"] {
            let mut body = b"a,b\n".to_vec();
            body.extend(row.repeat(2000));
            let (text, truncated, _) = parse_csv(&body, "", 100);
            assert!(truncated, "row {row:?}");
            assert!(text.chars().count() <= 100, "row {row:?}");
        }
    }

    #[test]
    fn test_csv_stops_reading_rows_when_render_budget_is_spent() {
        let mut body = b"a,b\n".to_vec();
        body.extend(b",\n".repeat(20_000));
        let (_, truncated, (rows, _)) = parse_csv(&body, "", 100);
        assert!(truncated);
        assert!(rows < 20, "read {rows} rows before truncating");
    }

    #[test]
    fn blank_lines_become_empty_rows_like_pythons_csv_reader() {
        // csv.reader("a,b\n\n\"x\ny\",z\r\n\r\n1,2\n\n") ->
        // [a,b], [], ["x\ny", z], [], [1,2], []
        let (text, _, (rows, cols)) = parse_csv(b"a,b\n\n\"x\ny\",z\r\n\r\n1,2\n\n", "", 2_000_000);
        assert_eq!((rows, cols), (6, 2), "{text}");
        let body: Vec<&str> = text.split("\n| --- | --- |\n").collect();
        assert_eq!(body[0], "| a | b |");
        assert!(body[1].starts_with("|  |  |\n| x"), "{text}");
        assert!(text.ends_with("| 1 | 2 |\n|  |  |"), "{text}");
    }

    #[test]
    fn wide_rows_are_clipped_and_flagged() {
        let row: Vec<String> = (0..40).map(|i| i.to_string()).collect();
        let body = format!("{}\n", row.join(","));
        let (text, truncated, (_, cols)) = parse_csv(body.as_bytes(), "", 2_000_000);
        assert!(truncated);
        assert_eq!(cols, 40);
        assert_eq!(text.lines().next().unwrap().matches(" | ").count(), 29);
    }

    #[test]
    fn sniffer_prefers_consistent_delimiters() {
        assert_eq!(sniff_delimiter("a;b;c\n1;2;3\n"), b';');
        assert_eq!(sniff_delimiter("a|b\n1|2\n"), b'|');
        assert_eq!(sniff_delimiter("\"x,y\";z\n\"1,2\";3\n"), b';');
        assert_eq!(sniff_delimiter("single column\nrows only\n"), b',');
        assert_eq!(sniff_delimiter(""), b',');
    }
}
