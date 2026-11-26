//! Date extraction and normalization.
//!
//! * [`extract_date_hint`] — port of `engines.base.extract_date_hint`
//!   (snippet/title text → `"YYYY-MM-DD"` or `"N units ago"`). The Python
//!   regexes use look-around, which the `regex` crate lacks; boundaries are
//!   checked explicitly with Python's rescanning semantics. "Today" is a
//!   parameter so results are deterministic. Digits are ASCII only.
//! * [`normalize_date`] — structured API date strings (ISO 8601, RFC 2822,
//!   month names, CJK, compact) → `YYYY-MM-DD` / `YYYY-MM` / `YYYY`,
//!   precision preserved (backs the manifest `parse_date` filter).
//! * [`hint_to_date`] — a hint back to an approximate calendar date.

use jiff::Span;
use jiff::civil::Date;
use regex::{Captures, Regex};
use std::sync::LazyLock;

static REL_EN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b([0-9]+)\s*(minute|hour|day|week|month|year)s?\s*ago\b").expect("regex")
});
static REL_CJK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"([0-9]{1,3})\s*(分钟|分鐘|分|時間|小时|小時|日|天|週間|周|週|か月|ヶ月|ケ月|个月|個月|年|분|시간|일|주|개월|년)\s*(?:前|전)",
    )
    .expect("regex")
});
static ISO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([0-9]{4})-([0-9]{2})-([0-9]{2})\b").expect("regex"));
static YMD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"([0-9]{4})(?:/([0-9]{1,2})/([0-9]{1,2})|\s*年\s*([0-9]{1,2})\s*月\s*([0-9]{1,2})\s*日)",
    )
    .expect("regex")
});
static MON_DAY_YEAR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)[a-z]*\s+([0-9]{1,2})(?:,\s*([0-9]{4}))?\b").expect("regex")
});
static SHORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([0-9]{1,2})/([0-9]{1,2})/([0-9]{2,4})\b").expect("regex"));

fn cjk_unit(u: &str) -> &'static str {
    match u {
        "分" | "分钟" | "分鐘" | "분" => "minute",
        "時間" | "小时" | "小時" | "시간" => "hour",
        "日" | "天" | "일" => "day",
        "週間" | "周" | "週" | "주" => "week",
        "年" | "년" => "year",
        _ => "month",
    }
}

fn is_cjk_sep(c: char) -> bool {
    c.is_whitespace() || matches!(c, '·' | '・' | '|' | ',' | '，' | '、' | '-')
}

/// Python `re.search` with look-around emulated by `ok(text, captures)`:
/// on a rejected candidate, retry from the next character.
fn search_with<'t>(
    re: &Regex,
    text: &'t str,
    ok: impl Fn(&Captures<'t>) -> bool,
) -> Vec<Captures<'t>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos <= text.len() {
        let Some(c) = re.captures_at(text, pos) else {
            break;
        };
        let m = c.get(0).expect("group 0");
        if ok(&c) {
            pos = if m.end() > m.start() {
                m.end()
            } else {
                m.end() + 1
            };
            out.push(c);
        } else {
            pos = m.start() + text[m.start()..].chars().next().map_or(1, char::len_utf8);
        }
    }
    out
}

fn char_before(text: &str, i: usize) -> Option<char> {
    text[..i].chars().next_back()
}

fn char_after(text: &str, i: usize) -> Option<char> {
    text[i..].chars().next()
}

fn plural(n: u64, unit: &str) -> String {
    format!("{unit}{}", if n == 1 { "" } else { "s" })
}

fn ymd(y: &str, m: &str, d: &str) -> Option<Date> {
    Date::new(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?).ok()
}

/// A page can have been published on `d`: year ≥ 1990 and not after
/// tomorrow (one day of time-zone slack).
pub fn plausible_publish_date(d: Date, today: Date) -> bool {
    d.year() >= 1990 && today.tomorrow().is_ok_and(|t| d <= t)
}

/// Normalised date hint found in `text`, or `None`.
///
/// Forms: `"YYYY-MM-DD"` for an unambiguous absolute date, `"N units ago"`
/// for a relative phrase (EN, ZH, JA, KO). Relative phrases win over absolute
/// dates; "today"/"yesterday" are deliberately ignored; partial dates without
/// a year are never completed; dates after tomorrow or before 1990 are skipped.
pub fn extract_date_hint(text: &str, today: Date) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    if let Some(c) = REL_EN.captures(text) {
        let raw = &c[1];
        let n: u64 = raw.parse().unwrap_or(u64::MAX);
        return Some(format!(
            "{raw} {} ago",
            plural(n, &c[2].to_ascii_lowercase())
        ));
    }
    let rel = search_with(&REL_CJK, text, |c| {
        let m = c.get(0).expect("group 0");
        char_before(text, m.start()).is_none_or(is_cjk_sep)
            && char_after(text, m.end()).is_none_or(is_cjk_sep)
    });
    if let Some(c) = rel.first() {
        let n: u64 = c[1].parse().ok()?;
        return Some(format!("{n} {} ago", plural(n, cjk_unit(&c[2]))));
    }
    for c in ISO.captures_iter(text) {
        if let Some(d) = ymd(&c[1], &c[2], &c[3]).filter(|d| plausible_publish_date(*d, today)) {
            return Some(d.to_string());
        }
    }
    let ymds = search_with(&YMD, text, |c| {
        let m = c.get(0).expect("group 0");
        let before_ok = char_before(text, m.start())
            .is_none_or(|ch| !(ch.is_ascii_alphanumeric() || ch == '.'));
        let after_ok = c.get(2).is_none()
            || char_after(text, m.end()).is_none_or(|ch| !(ch.is_ascii_digit() || ch == '/'));
        before_ok && after_ok
    });
    for c in &ymds {
        let (m, d) = match (c.get(2), c.get(3)) {
            (Some(m), Some(d)) => (m.as_str(), d.as_str()),
            _ => (
                c.get(4).map_or("", |x| x.as_str()),
                c.get(5).map_or("", |x| x.as_str()),
            ),
        };
        if let Some(d) = ymd(&c[1], m, d).filter(|d| plausible_publish_date(*d, today)) {
            return Some(d.to_string());
        }
    }
    for c in MON_DAY_YEAR.captures_iter(text) {
        let Some(year) = c.get(3) else { continue };
        let month = month_number(&c[1])?;
        if let Some(d) = ymd(year.as_str(), &month.to_string(), &c[2])
            .filter(|d| plausible_publish_date(*d, today))
        {
            return Some(d.to_string());
        }
    }
    if let Some(c) = SHORT.captures(text) {
        // %m/%d/%Y, %m/%d/%y, %d/%m/%Y, %d/%m/%y (Python strptime: %Y = 4 digits, %y = 2).
        let (a, b, y) = (&c[1], &c[2], &c[3]);
        let year = match y.len() {
            4 => y.parse::<i16>().ok(),
            2 => y
                .parse::<i16>()
                .ok()
                .map(|v| if v < 69 { 2000 + v } else { 1900 + v }),
            _ => None,
        };
        if let Some(year) = year {
            for (m, d) in [(a, b), (b, a)] {
                if let Some(date) =
                    ymd(&year.to_string(), m, d).filter(|d| plausible_publish_date(*d, today))
                {
                    return Some(date.to_string());
                }
            }
        }
    }
    None
}

fn month_number(name: &str) -> Option<i8> {
    let n = match name.get(..3)?.to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    Some(n)
}

static REL_HINT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([0-9]+) (minute|hour|day|week|month|year)s? ago$").expect("regex")
});

/// The calendar date a hint denotes (relative hints count back from `today`;
/// minutes and hours resolve to `today`).
pub fn hint_to_date(hint: &str, today: Date) -> Option<Date> {
    if let Some(c) = REL_HINT.captures(hint.trim()) {
        let n: i64 = c[1].parse().ok()?;
        let span = match &c[2] {
            "minute" | "hour" => return Some(today),
            "day" => Span::new().try_days(n).ok()?,
            "week" => Span::new().try_weeks(n).ok()?,
            "month" => Span::new().try_months(n).ok()?,
            _ => Span::new().try_years(n).ok()?,
        };
        return today.checked_sub(span).ok();
    }
    let n = normalize_date(hint)?;
    let parts: Vec<&str> = n.split('-').collect();
    match parts.as_slice() {
        [y, m, d] => ymd(y, m, d),
        _ => None,
    }
}

static N_ISO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([0-9]{4})-([0-9]{1,2})-([0-9]{1,2})(?:$|[T ]|[+-][0-9]{2}:?[0-9]{2}$|Z$)")
        .expect("regex")
});
static N_YM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([0-9]{4})-([0-9]{1,2})$").expect("regex"));
static N_Y: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([0-9]{4})$").expect("regex"));
static N_SLASH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([0-9]{4})/([0-9]{1,2})/([0-9]{1,2})\b").expect("regex"));
static N_COMPACT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([0-9]{4})([0-9]{2})([0-9]{2})$").expect("regex"));
static N_CJK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([0-9]{4})\s*年\s*([0-9]{1,2})\s*月(?:\s*([0-9]{1,2})\s*日)?").expect("regex")
});
static N_MDY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?i)(?:[a-z]{3,9},?\s+)?([a-z]{3,9})\.?\s+([0-9]{1,2})(?:st|nd|rd|th)?,?\s+([0-9]{4})\b",
    )
    .expect("regex")
});
static N_DMY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?i)(?:[a-z]{3,9},?\s+)?([0-9]{1,2})(?:st|nd|rd|th)?\s+([a-z]{3,9})\.?,?\s+([0-9]{4})\b",
    )
    .expect("regex")
});
static N_MY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?i)([a-z]{3,9})\.?\s+([0-9]{4})$").expect("regex"));

fn month_from_name(name: &str) -> Option<i8> {
    let lower = name.to_ascii_lowercase();
    let n = month_number(&lower)?;
    // Accept the abbreviation or the full English name only.
    const FULL: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    let full = FULL[(n - 1) as usize];
    (lower.len() == 3 || lower == full || lower == "sept").then_some(n)
}

/// Normalise a structured date string to `YYYY-MM-DD`, `YYYY-MM` or
/// `YYYY` (calendar date as written; offsets are not converted). `None`
/// when it is not a recognisable, valid date.
pub fn normalize_date(input: &str) -> Option<String> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    let full = |y: &str, m: &str, d: &str| ymd(y, m, d).map(|d| d.to_string());
    if let Some(c) = N_ISO.captures(s) {
        return full(&c[1], &c[2], &c[3]);
    }
    if let Some(c) = N_YM.captures(s) {
        let m: i8 = c[2].parse().ok()?;
        return (1..=12).contains(&m).then(|| format!("{}-{m:02}", &c[1]));
    }
    if N_Y.is_match(s) {
        return Some(s.to_owned());
    }
    if let Some(c) = N_SLASH.captures(s) {
        return full(&c[1], &c[2], &c[3]);
    }
    if let Some(c) = N_COMPACT.captures(s) {
        return full(&c[1], &c[2], &c[3]);
    }
    if let Some(c) = N_CJK.captures(s) {
        return match c.get(3) {
            Some(d) => full(&c[1], &c[2], d.as_str()),
            None => {
                let m: i8 = c[2].parse().ok()?;
                (1..=12).contains(&m).then(|| format!("{}-{m:02}", &c[1]))
            }
        };
    }
    if let Ok(z) = jiff::fmt::rfc2822::parse(s) {
        return Some(z.date().to_string());
    }
    if let Some(c) = N_MDY.captures(s) {
        let m = month_from_name(&c[1])?;
        return full(&c[3], &m.to_string(), &c[2]);
    }
    if let Some(c) = N_DMY.captures(s) {
        let m = month_from_name(&c[2])?;
        return full(&c[3], &m.to_string(), &c[1]);
    }
    if let Some(c) = N_MY.captures(s) {
        let m = month_from_name(&c[1])?;
        return Some(format!("{}-{m:02}", &c[2]));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    const TODAY: Date = date(2026, 11, 25);

    fn hint(t: &str) -> String {
        extract_date_hint(t, TODAY).unwrap_or_default()
    }

    #[test]
    fn python_vectors_english() {
        // tests/test_dates.py
        assert_eq!(hint("Apr 28, 2026 — OpenAI announced…"), "2026-04-28");
        assert_eq!(hint("2024-12-01: release notes for…"), "2024-12-01");
        assert_eq!(hint("2 days ago - Reuters reports…"), "2 days ago");
        assert_eq!(hint("1 day ago, Reuters reports…"), "1 day ago");
        assert_eq!(hint("3 hours ago: breaking news"), "3 hours ago");
        assert_eq!(hint("April 28, 2026: a press release"), "2026-04-28");
        assert_eq!(hint("Apr 28 — undated note"), "");
        assert_eq!(hint(""), "");
        assert_eq!(hint("just some random text without a date"), "");
        assert_eq!(hint("Today's update from the team"), "");
        assert_eq!(hint("Yesterday's blog post"), "");
        assert_eq!(hint("Apr 28, 2026 (2 days ago) — story"), "2 days ago");
    }

    #[test]
    fn python_vectors_future_dates_are_skipped() {
        let future = TODAY.checked_add(Span::new().days(40)).unwrap();
        let long = future.strftime("%B %d, %Y").to_string();
        assert_eq!(
            hint(&format!("FastAPI Conf is happening on {long} in Amsterdam")),
            ""
        );
        assert_eq!(
            hint(&format!("Conf on {long}. Posted Apr 28, 2026.")),
            "2026-04-28"
        );
        assert_eq!(
            hint(&format!("deadline {future}; published 2026-01-05")),
            "2026-01-05"
        );
        let tomorrow = TODAY.tomorrow().unwrap();
        assert_eq!(hint(&format!("posted {tomorrow}")), tomorrow.to_string());
    }

    #[test]
    fn python_vectors_east_asian() {
        for (text, want) in [
            ("2026/10/1 - Python 3.15 の新機能", "2026-10-01"),
            ("2026年10月1日に公開", "2026-10-01"),
            ("5日前 - 記事", "5 days ago"),
            ("1時間前", "1 hour ago"),
            ("3天前 发布", "3 days ago"),
            ("5小时前", "5 hours ago"),
            ("2个月前", "2 months ago"),
            ("3일 전", "3 days ago"),
            ("2시간 전", "2 hours ago"),
            ("请于2026年10月1日前提交", "2026-10-01"),
        ] {
            assert_eq!(hint(text), want, "{text}");
        }
    }

    #[test]
    fn python_vectors_year_first_lookalikes() {
        for text in [
            "version 3.11.2",
            "certifi 2024.08.30.1",
            "2026/13/40",
            "2026/10/1/2",
            "2999/01/01",
            "数年前",
            "请于10月1日前提交",
            "10월 1일 전까지 신청",
            "2024年前半の動向",
            "2020年前后",
            "PyCharm 2023.3.4 is out",
            "v2024.1.2 release",
            "certifi 2024.08.30",
            "Upgrade to certifi 2024.08.30.",
            "この寺は30年前に建てられた",
            "締切の3日前までに申し込み",
        ] {
            assert_eq!(hint(text), "", "{text}");
        }
    }

    #[test]
    fn short_numeric_dates() {
        assert_eq!(hint("posted 3/4/2025"), "2025-03-04");
        assert_eq!(hint("posted 1/2/24"), "2024-01-02");
        assert_eq!(hint("posted 25/12/2024"), "2024-12-25");
        assert_eq!(hint("v1/2/245"), "");
    }

    #[test]
    fn hints_resolve_to_dates() {
        assert_eq!(hint_to_date("2 days ago", TODAY), Some(date(2026, 11, 23)));
        assert_eq!(hint_to_date("1 month ago", TODAY), Some(date(2026, 10, 25)));
        assert_eq!(hint_to_date("5 hours ago", TODAY), Some(TODAY));
        assert_eq!(hint_to_date("2024-12-01", TODAY), Some(date(2024, 12, 1)));
        assert_eq!(hint_to_date("99999999999 years ago", TODAY), None);
        assert_eq!(hint_to_date("nonsense", TODAY), None);
    }

    #[test]
    fn normalize_structured_dates() {
        for (input, want) in [
            ("2024-10-01", Some("2024-10-01")),
            ("2024-10-01T23:30:00-05:00", Some("2024-10-01")),
            ("2024-10-01T10:00:00Z", Some("2024-10-01")),
            ("2024-10-01 10:00:00", Some("2024-10-01")),
            ("2024-9-5", Some("2024-09-05")),
            ("2024-10", Some("2024-10")),
            ("2024", Some("2024")),
            ("2024/10/01", Some("2024-10-01")),
            ("20241001", Some("2024-10-01")),
            ("2024年10月1日", Some("2024-10-01")),
            ("2024年10月", Some("2024-10")),
            ("Tue, 01 Oct 2024 10:00:00 GMT", Some("2024-10-01")),
            ("Oct 1, 2024", Some("2024-10-01")),
            ("October 1st, 2024", Some("2024-10-01")),
            ("1 October 2024", Some("2024-10-01")),
            ("Sept 3, 2024", Some("2024-09-03")),
            ("Oct 2024", Some("2024-10")),
            ("2024-02-30", None),
            ("2024-13", None),
            ("Octember 1, 2024", None),
            ("soon", None),
            ("", None),
        ] {
            assert_eq!(normalize_date(input).as_deref(), want, "{input}");
        }
    }
}
