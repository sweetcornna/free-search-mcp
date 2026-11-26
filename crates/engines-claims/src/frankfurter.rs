//! Frankfurter: ECB daily reference exchange rate between two currencies,
//! falling back to open.er-api.com for non-ECB currencies.
//!
//! Port of `src/search_mcp/engines/frankfurter.py`.

use crate::facts::{clip_query, fmt_comma_fixed, fmt_g, iso_day};
use crate::http::{
    CallBudget, build_source_decl, get_json, get_json_opt, hits_output, make_hit,
    parse_search_params,
};
use fsm_core::{
    Availability, Config, FsmError, Hit, PoolRole, ProbeSpec, Source, SourceDecl, SourceDeps,
    SourceLimits, SrcCx, SrcOutput,
};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::LazyLock;

const API_URL: &str = "https://api.frankfurter.dev/v1/latest";
const OPEN_API_URL: &str = "https://open.er-api.com/v6/latest";

/// Currencies published by the ECB (Frankfurter).
pub const CURRENCIES: &[&str] = &[
    "AUD", "BGN", "BRL", "CAD", "CHF", "CNY", "CZK", "DKK", "EUR", "GBP", "HKD", "HUF", "IDR",
    "ILS", "INR", "ISK", "JPY", "KRW", "MXN", "MYR", "NOK", "NZD", "PHP", "PLN", "RON", "SEK",
    "SGD", "THB", "TRY", "USD", "ZAR",
];

/// Additional ISO currency codes served by the open ExchangeRate-API endpoint.
pub const MORE_CURRENCIES: &[&str] = &[
    "TWD", "VND", "RUB", "AED", "SAR", "MOP", "EGP", "PKR", "BDT", "LKR", "NPR", "KWD", "QAR",
    "OMR", "BHD", "JOD", "IQD", "IRR", "KZT", "UZS", "UAH", "BYN", "GEL", "AMD", "AZN", "MNT",
    "KHR", "LAK", "MMK", "ARS", "CLP", "COP", "PEN", "UYU", "BOB", "VES", "DOP", "GTQ", "CRC",
    "JMD", "TTD", "NGN", "KES", "GHS", "TZS", "UGX", "ETB", "MAD", "DZD", "TND", "XOF", "XAF",
    "RSD", "MKD", "BAM", "ALL", "MDL", "LBP", "SYP", "AFN", "BTN", "MVR", "FJD", "PGK", "XPF",
    "BND", "MUR", "SCR", "ZMW", "MZN", "AOA", "BWP", "NAD", "CUP", "HTG", "HNL", "NIO", "PAB",
    "PYG", "SVC", "BZD", "BBD", "BSD", "GYD", "SRD", "KGS", "TJS", "TMT", "LYD", "SDG", "SOS",
    "RWF", "BIF", "MGA", "MWK", "GNF", "SLE", "LRD", "GMD", "CVE", "STN", "SZL", "LSL", "ERN",
    "DJF", "KMF", "MRU", "CDF", "XCD", "AWG", "ANG", "KYD", "BMD", "GIP", "FKP", "SHP", "JEP",
    "GGP", "IMP", "TVD", "WST", "TOP", "VUV", "SBD", "KID", "FOK", "ZWL",
];

static ECB_SET: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| CURRENCIES.iter().copied().collect());
static ALL_CODES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    CURRENCIES
        .iter()
        .chain(MORE_CURRENCIES.iter())
        .copied()
        .collect()
});

const RAW_NAMES: &[(&str, &str)] = &[
    ("dollar", "USD"),
    ("dollars", "USD"),
    ("usd", "USD"),
    ("美元", "USD"),
    ("美金", "USD"),
    ("euro", "EUR"),
    ("euros", "EUR"),
    ("欧元", "EUR"),
    ("yuan", "CNY"),
    ("rmb", "CNY"),
    ("renminbi", "CNY"),
    ("人民币", "CNY"),
    ("元", "CNY"),
    ("yen", "JPY"),
    ("日元", "JPY"),
    ("日币", "JPY"),
    ("pound", "GBP"),
    ("pounds", "GBP"),
    ("sterling", "GBP"),
    ("英镑", "GBP"),
    ("hong kong dollar", "HKD"),
    ("港币", "HKD"),
    ("港元", "HKD"),
    ("franc", "CHF"),
    ("francs", "CHF"),
    ("瑞郎", "CHF"),
    ("瑞士法郎", "CHF"),
    ("won", "KRW"),
    ("韩元", "KRW"),
    ("rupee", "INR"),
    ("rupees", "INR"),
    ("卢比", "INR"),
    ("singapore dollar", "SGD"),
    ("新加坡元", "SGD"),
    ("新币", "SGD"),
    ("australian dollar", "AUD"),
    ("澳元", "AUD"),
    ("canadian dollar", "CAD"),
    ("加元", "CAD"),
    ("baht", "THB"),
    ("泰铢", "THB"),
    ("ringgit", "MYR"),
    ("林吉特", "MYR"),
    ("rupiah", "IDR"),
    ("印尼盾", "IDR"),
    ("lira", "TRY"),
    ("里拉", "TRY"),
    ("krona", "SEK"),
    ("kronor", "SEK"),
    ("瑞典克朗", "SEK"),
    ("krone", "NOK"),
    ("kroner", "NOK"),
    ("挪威克朗", "NOK"),
    ("zloty", "PLN"),
    ("兹罗提", "PLN"),
    ("peso", "MXN"),
    ("比索", "MXN"),
    ("real", "BRL"),
    ("reais", "BRL"),
    ("雷亚尔", "BRL"),
    ("rand", "ZAR"),
    ("兰特", "ZAR"),
    ("new zealand dollar", "NZD"),
    ("新西兰元", "NZD"),
    ("台币", "TWD"),
    ("新台币", "TWD"),
    ("taiwan dollar", "TWD"),
    ("越南盾", "VND"),
    ("dong", "VND"),
    ("卢布", "RUB"),
    ("rouble", "RUB"),
    ("ruble", "RUB"),
    ("迪拉姆", "AED"),
    ("dirham", "AED"),
    ("里亚尔", "SAR"),
    ("riyal", "SAR"),
    ("澳门元", "MOP"),
    ("pataca", "MOP"),
    ("埃及镑", "EGP"),
    ("格里夫纳", "UAH"),
    ("hryvnia", "UAH"),
    ("菲律宾比索", "PHP"),
    ("阿根廷比索", "ARS"),
    ("奈拉", "NGN"),
    ("naira", "NGN"),
];

static NAMES_BY_LEN: LazyLock<Vec<(&'static str, &'static str)>> = LazyLock::new(|| {
    let mut v = RAW_NAMES.to_vec();
    v.sort_by_key(|a| std::cmp::Reverse(a.0.chars().count()));
    v
});

// Match only decimal digits we explicitly convert to ASCII (ASCII, full-width,
// Arabic-Indic, Extended Arabic-Indic, Devanagari) so Unicode digits like
// full-width `１００` parse identically to Python `float()` while unhandled
// Unicode scripts don't abort currency parsing.
const DIGIT_CLASS: &str =
    r"[0-9\x{FF10}-\x{FF19}\x{0660}-\x{0669}\x{06F0}-\x{06F9}\x{0966}-\x{096F}]";

static RAW_NUM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&r"[+-]?(?:D+(?:[.,]D+)*|\.D+)".replace('D', DIGIT_CLASS)).expect("raw num regex")
});
static NUMBER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[+-]?(?:(?:[0-9]{1,3}(?:,[0-9]{3})+|[0-9]+)(?:\.[0-9]+)?|\.[0-9]+)$")
        .expect("number regex")
});
static CODE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([A-Za-z]{3})\b").expect("code regex"));

fn ascii_digits(s: &str) -> String {
    s.chars()
        .map(|c| {
            let base = match c {
                '\u{FF10}'..='\u{FF19}' => 0xFF10, // full-width
                '\u{0660}'..='\u{0669}' => 0x0660, // Arabic-Indic
                '\u{06F0}'..='\u{06F9}' => 0x06F0, // Extended Arabic-Indic
                '\u{0966}'..='\u{096F}' => 0x0966, // Devanagari
                _ => return c,
            };
            char::from(b'0' + (c as u32 - base) as u8)
        })
        .collect()
}

struct AmountMatch<'a> {
    number: &'a str,
    multiplier: Option<&'static str>,
    currency: Option<(usize, String)>,
}

fn is_amount_preceder(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-')
}

/// Match a known 3-letter ISO currency code at the start of `s` not followed by an ASCII letter.
fn match_currency_code_at(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || !bytes[1].is_ascii_alphabetic()
        || !bytes[2].is_ascii_alphabetic()
    {
        return None;
    }
    if bytes.len() > 3 && bytes[3].is_ascii_alphabetic() {
        return None;
    }
    let upper = s[..3].to_ascii_uppercase();
    ALL_CODES.contains(upper.as_str()).then_some(upper)
}

fn skip_ws(s: &str, mut pos: usize) -> usize {
    while let Some(ch) = s[pos..].chars().next() {
        if ch.is_whitespace() {
            pos += ch.len_utf8();
        } else {
            break;
        }
    }
    pos
}

/// Look-around-free exact equivalent of Python `frankfurter._AMOUNT_RE.search(query)`.
fn find_amount_match(query: &str) -> Option<AmountMatch<'_>> {
    let bytes = query.as_bytes();
    let mut pos = 0usize;
    let num_match = loop {
        if pos > query.len() {
            return None;
        }
        let m = RAW_NUM_RE.find_at(query, pos)?;
        let before_ok = m.start() == 0 || !is_amount_preceder(bytes[m.start() - 1]);
        if before_ok {
            break m;
        }
        pos = m.start() + query[m.start()..].chars().next().map_or(1, char::len_utf8);
    };

    let mut cur = num_match.end();
    let mut multiplier: Option<&'static str> = None;

    let ws_pos = skip_ws(query, cur);
    let rest = &query[ws_pos..];
    if rest.starts_with('万') {
        multiplier = Some("万");
        cur = ws_pos + '万'.len_utf8();
    } else if rest.starts_with('亿') {
        multiplier = Some("亿");
        cur = ws_pos + '亿'.len_utf8();
    } else {
        // ASCII multipliers: "thousand", "million", "k" with lookahead
        // `(?=(?:{_CURRENCY_CODES})(?![a-z])|[^a-z]|$)`.
        for word in ["thousand", "million", "k"] {
            if rest.len() >= word.len()
                && rest.is_char_boundary(word.len())
                && rest[..word.len()].eq_ignore_ascii_case(word)
            {
                let after = &rest[word.len()..];
                let lookahead_ok = match after.as_bytes().first() {
                    None => true,
                    Some(&b) if !b.is_ascii_alphabetic() => true,
                    Some(_) => match_currency_code_at(after).is_some(),
                };
                if lookahead_ok {
                    multiplier = Some(word);
                    cur = ws_pos + word.len();
                    break;
                }
            }
        }
    }

    let ws_cur = skip_ws(query, cur);
    let currency = match_currency_code_at(&query[ws_cur..]).map(|code| (ws_cur, code));

    Some(AmountMatch {
        number: num_match.as_str(),
        multiplier,
        currency,
    })
}

fn multiplier_value(m: Option<&str>) -> f64 {
    match m {
        Some("k") | Some("thousand") => 1_000.0,
        Some("万") => 10_000.0,
        Some("million") => 1_000_000.0,
        Some("亿") => 100_000_000.0,
        _ => 1.0,
    }
}

/// `(base, quote, amount)` from a query, or `None` when it names fewer than two
/// currencies or carries an invalid/non-finite amount.
pub fn parse_pair(query: &str) -> Option<(String, String, f64)> {
    let mut hits: Vec<(usize, String)> = Vec::new();
    let amount_match = find_amount_match(query);
    if let Some(ref am) = amount_match
        && let Some((pos, ref code)) = am.currency
    {
        hits.push((pos, code.clone()));
    }
    for cap in CODE_RE.captures_iter(query) {
        if let Some(m) = cap.get(1) {
            let code = m.as_str().to_ascii_uppercase();
            if ALL_CODES.contains(code.as_str()) {
                hits.push((m.start(), code));
            }
        }
    }
    let mut lowered = query.to_lowercase();
    for &(name, code) in NAMES_BY_LEN.iter() {
        if let Some(at) = lowered.find(name) {
            if !code.is_empty() {
                hits.push((at, code.to_owned()));
            }
            let spaces = " ".repeat(name.len());
            lowered = lowered.replace(name, &spaces);
        }
    }
    hits.sort_by_key(|(at, _)| *at);
    let mut found: Vec<String> = Vec::new();
    for (_, code) in hits {
        if !found.contains(&code) {
            found.push(code);
        }
    }
    if found.len() < 2 {
        return None;
    }

    let mut amount = 1.0_f64;
    if let Some(am) = amount_match {
        let number = ascii_digits(am.number);
        if !NUMBER_RE.is_match(&number) {
            return None;
        }
        let parsed: f64 = number.replace(',', "").parse().ok()?;
        amount = parsed * multiplier_value(am.multiplier);
        if !amount.is_finite() {
            return None;
        }
    }

    Some((found[0].clone(), found[1].clone(), amount))
}

/// Reshape an `open.er-api.com` response into Frankfurter's `{base, date, rates}` shape.
pub fn from_open_api(raw: &Value, symbol: &str) -> Option<Value> {
    let obj = raw.as_object()?;
    if obj.get("result").and_then(Value::as_str) != Some("success") {
        return None;
    }
    let rates = obj.get("rates").and_then(Value::as_object)?;
    let rate = rates.get(symbol)?;
    let day = iso_day(obj.get("time_last_update_unix"));
    let date_str = if !day.is_empty() {
        day
    } else {
        obj.get("time_last_update_utc")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(16)
            .collect()
    };
    Some(json!({
        "base": obj.get("base_code").cloned().unwrap_or(Value::Null),
        "date": date_str,
        "rates": { symbol: rate.clone() },
        "_source": "open-er-api",
    }))
}

pub struct FrankfurterEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl FrankfurterEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let decl = build_source_decl(
            "frankfurter",
            "Frankfurter (ECB FX)",
            "ECB daily reference rate between two currencies; others via ExchangeRate-API.",
            &["ecb", "fx", "currency", "exchange rate", "汇率"],
            "https://frankfurter.dev",
            &["finance"],
            PoolRole::DirectClaimant,
            SourceLimits::new(2.0, 4, 2, 5_000, 3_600),
            r#"source.call("frankfurter.search", {query: "100 usd to cny"})"#,
            ProbeSpec::new(json!({"query": "100 usd to cny", "limit": 3}), 1),
        );
        Self { decl, deps }
    }

    pub fn build_url(query: &str) -> String {
        match parse_pair(query) {
            Some((base, symbol, _)) => format!("{API_URL}?base={base}&symbols={symbol}"),
            None => format!("{API_URL}?base=USD"),
        }
    }

    pub fn map_results(&self, payload: &Value) -> Vec<Hit> {
        let Some(obj) = payload.as_object() else {
            return Vec::new();
        };
        let Some(base) = obj.get("base").and_then(Value::as_str) else {
            return Vec::new();
        };
        let Some(rates) = obj.get("rates").and_then(Value::as_object) else {
            return Vec::new();
        };
        if rates.is_empty() {
            return Vec::new();
        }
        let day = obj.get("date").and_then(Value::as_str);
        let day_display = day.unwrap_or("");
        let amount = obj
            .get("_amount")
            .and_then(Value::as_f64)
            .filter(|a| a.is_finite())
            .unwrap_or(1.0);
        let is_open_api = obj.get("_source").and_then(Value::as_str) == Some("open-er-api");
        let custom_url = obj.get("_url").and_then(Value::as_str);

        let mut results = Vec::new();
        for (symbol, raw_rate) in rates {
            let Some(rate) = raw_rate
                .as_f64()
                .or_else(|| raw_rate.as_str().and_then(|s| s.parse::<f64>().ok()))
                .filter(|r| r.is_finite())
            else {
                continue;
            };
            let converted = if amount != 1.0 {
                format!(
                    "{} {base} = {} {symbol}",
                    fmt_g(amount, 6),
                    fmt_comma_fixed(amount * rate, 4)
                )
            } else {
                String::new()
            };
            let (source_label, mut snippet) = if is_open_api {
                (
                    "ExchangeRate-API",
                    format!(
                        "1 {base} = {} {symbol} · daily rate for {day_display} from the open ExchangeRate-API endpoint, updated once a day; not a live market quote · Rates By Exchange Rate API https://www.exchangerate-api.com",
                        fmt_g(rate, 6)
                    ),
                )
            } else {
                (
                    "ECB",
                    format!(
                        "1 {base} = {} {symbol} · ECB reference rate for {day_display}, published once per working day around 16:00 CET; not a live market quote",
                        fmt_g(rate, 6)
                    ),
                )
            };
            if !converted.is_empty() {
                snippet = format!("{converted} · {snippet}");
            }
            let title = format!(
                "{base} to {symbol}: {} ({source_label}, {day_display})",
                fmt_g(rate, 6)
            );
            let url = custom_url
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{API_URL}?base={base}&symbols={symbol}"));
            results.push(make_hit(
                &self.decl.id,
                title,
                url,
                snippet,
                day.map(str::to_owned),
            ));
        }
        results
    }
}

#[async_trait::async_trait]
impl Source for FrankfurterEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn claims(&self, query: &str, _lang: Option<&str>) -> f32 {
        if parse_pair(clip_query(query)).is_some() {
            1.0
        } else {
            0.0
        }
    }

    fn availability(&self, _cfg: &Config) -> Availability {
        Availability::Available
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let (query, limit) = parse_search_params(&self.decl.id, op, &params, 10, 50)?;
        if query.trim().is_empty() {
            return Ok(hits_output(Vec::new()));
        }
        let Some((base, symbol, amount)) = parse_pair(&query) else {
            return Ok(hits_output(Vec::new()));
        };
        let budget = CallBudget::from_cx(cx, &self.decl);
        let (url, payload) = if ECB_SET.contains(base.as_str()) && ECB_SET.contains(symbol.as_str())
        {
            let url = format!("{API_URL}?base={base}&symbols={symbol}");
            let raw = get_json(&self.deps, &self.decl, cx, &budget, &url, &[], None).await?;
            (url, raw)
        } else {
            let url = format!("{OPEN_API_URL}/{base}");
            let raw = get_json_opt(&self.deps, &self.decl, cx, &budget, &url, &[], None).await?;
            let reshaped = raw.and_then(|v| from_open_api(&v, &symbol));
            (url, reshaped)
        };
        let Some(mut payload) = payload else {
            return Ok(hits_output(Vec::new()));
        };
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("_amount".to_owned(), json!(amount));
            obj.insert("_url".to_owned(), json!(url));
        }
        let mut hits = self.map_results(&payload);
        hits.truncate(limit);
        Ok(hits_output(hits))
    }
}
