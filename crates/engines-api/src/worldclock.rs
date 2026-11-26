//! Local World Clock engine: current date & time in named cities or IANA zones.
//!
//! Uses `TransportProfile::Local` and `jiff`'s IANA timezone database — zero network calls.

use crate::common::{make_hit, parse_limit, search_op};
use crate::facts::query_is_chinese;
use async_trait::async_trait;
use fsm_core::{
    AuthMode, Capability, FsmError, Hit, HitKind, OpDecl, Param, ParamType, PoolRole, ReturnKind,
    Source, SourceDecl, SourceLimits, SrcCx, SrcOutput, TransportProfile,
};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

static TIME_ASK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(what time|current time|local time|time now|time zone|timezone|utc offset|what(?:'s| is) the (?:date|time)|today'?s date|what day is it)\b|几点|现在时间|当地时间|北京时间|时区|时差|今天几号|今天是|现在是|什么时候",
    )
    .expect("regex")
});

static IANA_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b((?:Africa|America|Antarctica|Asia|Atlantic|Australia|Europe|Indian|Pacific)/[A-Za-z_]+(?:/[A-Za-z_]+)?)\b",
    )
    .expect("regex")
});

struct ZoneTable {
    by_key: HashMap<String, (&'static str, &'static str)>,
    keys_by_len_desc: Vec<(String, bool)>, // (key, is_ascii_word)
    zh_name: HashMap<&'static str, &'static str>,
}

fn contains_cjk(s: &str) -> bool {
    s.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c))
}

fn is_ascii_key(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'+' || b == b' ')
}

const ZONE_ROWS: &[(&str, &str, &[&str])] = &[
    (
        "Asia/Shanghai",
        "Beijing",
        &[
            "北京",
            "上海",
            "shanghai",
            "china",
            "中国",
            "北京时间",
            "cst",
            "广州",
            "深圳",
            "杭州",
            "成都",
            "重庆",
            "武汉",
            "南京",
            "西安",
            "天津",
        ],
    ),
    ("Asia/Hong_Kong", "Hong Kong", &["香港"]),
    ("Asia/Taipei", "Taipei", &["台北", "台湾", "taiwan"]),
    (
        "Asia/Tokyo",
        "Tokyo",
        &["东京", "japan", "日本", "大阪", "osaka", "jst"],
    ),
    ("Asia/Seoul", "Seoul", &["首尔", "korea", "韩国"]),
    ("Asia/Singapore", "Singapore", &["新加坡"]),
    (
        "Asia/Bangkok",
        "Bangkok",
        &[
            "曼谷", "thailand", "泰国", "越南", "vietnam", "hanoi", "河内",
        ],
    ),
    ("Asia/Jakarta", "Jakarta", &["雅加达", "indonesia", "印尼"]),
    (
        "Asia/Kuala_Lumpur",
        "Kuala Lumpur",
        &["吉隆坡", "malaysia", "马来西亚"],
    ),
    (
        "Asia/Manila",
        "Manila",
        &["马尼拉", "philippines", "菲律宾"],
    ),
    (
        "Asia/Kolkata",
        "New Delhi",
        &["新德里", "india", "印度", "mumbai", "孟买", "ist"],
    ),
    (
        "Asia/Dubai",
        "Dubai",
        &["迪拜", "uae", "阿联酋", "abu dhabi"],
    ),
    ("Asia/Riyadh", "Riyadh", &["利雅得", "saudi arabia", "沙特"]),
    ("Asia/Tehran", "Tehran", &["德黑兰", "iran", "伊朗"]),
    (
        "Asia/Karachi",
        "Karachi",
        &["卡拉奇", "pakistan", "巴基斯坦"],
    ),
    ("Asia/Dhaka", "Dhaka", &["达卡", "bangladesh", "孟加拉"]),
    (
        "Asia/Almaty",
        "Almaty",
        &["阿拉木图", "kazakhstan", "哈萨克斯坦"],
    ),
    (
        "Europe/Moscow",
        "Moscow",
        &["莫斯科", "russia", "俄罗斯", "msk"],
    ),
    (
        "Europe/Istanbul",
        "Istanbul",
        &["伊斯坦布尔", "turkey", "土耳其"],
    ),
    (
        "Europe/London",
        "London",
        &["伦敦", "uk", "britain", "英国", "gmt", "bst"],
    ),
    ("Europe/Dublin", "Dublin", &["都柏林", "ireland", "爱尔兰"]),
    (
        "Europe/Paris",
        "Paris",
        &["巴黎", "france", "法国", "cet", "cest"],
    ),
    (
        "Europe/Berlin",
        "Berlin",
        &[
            "柏林",
            "germany",
            "德国",
            "munich",
            "慕尼黑",
            "frankfurt",
            "法兰克福",
        ],
    ),
    (
        "Europe/Amsterdam",
        "Amsterdam",
        &["阿姆斯特丹", "netherlands", "荷兰"],
    ),
    (
        "Europe/Brussels",
        "Brussels",
        &["布鲁塞尔", "belgium", "比利时"],
    ),
    (
        "Europe/Zurich",
        "Zurich",
        &["苏黎世", "switzerland", "瑞士", "geneva", "日内瓦"],
    ),
    (
        "Europe/Madrid",
        "Madrid",
        &["马德里", "spain", "西班牙", "barcelona", "巴塞罗那"],
    ),
    (
        "Europe/Rome",
        "Rome",
        &["罗马", "italy", "意大利", "milan", "米兰"],
    ),
    ("Europe/Vienna", "Vienna", &["维也纳", "austria", "奥地利"]),
    (
        "Europe/Stockholm",
        "Stockholm",
        &["斯德哥尔摩", "sweden", "瑞典"],
    ),
    ("Europe/Oslo", "Oslo", &["奥斯陆", "norway", "挪威"]),
    (
        "Europe/Copenhagen",
        "Copenhagen",
        &["哥本哈根", "denmark", "丹麦"],
    ),
    (
        "Europe/Helsinki",
        "Helsinki",
        &["赫尔辛基", "finland", "芬兰"],
    ),
    ("Europe/Warsaw", "Warsaw", &["华沙", "poland", "波兰"]),
    ("Europe/Prague", "Prague", &["布拉格", "czechia", "捷克"]),
    ("Europe/Athens", "Athens", &["雅典", "greece", "希腊"]),
    ("Europe/Lisbon", "Lisbon", &["里斯本", "portugal", "葡萄牙"]),
    ("Europe/Kyiv", "Kyiv", &["基辅", "ukraine", "乌克兰"]),
    ("Africa/Cairo", "Cairo", &["开罗", "egypt", "埃及"]),
    (
        "Africa/Johannesburg",
        "Johannesburg",
        &["约翰内斯堡", "south africa", "南非"],
    ),
    ("Africa/Lagos", "Lagos", &["拉各斯", "nigeria", "尼日利亚"]),
    ("Africa/Nairobi", "Nairobi", &["内罗毕", "kenya", "肯尼亚"]),
    (
        "America/New_York",
        "New York",
        &[
            "纽约",
            "washington",
            "华盛顿",
            "boston",
            "波士顿",
            "miami",
            "迈阿密",
            "eastern time",
            "美东",
            "美国东部",
            "est",
            "edt",
        ],
    ),
    (
        "America/Chicago",
        "Chicago",
        &[
            "芝加哥",
            "houston",
            "休斯顿",
            "dallas",
            "达拉斯",
            "central time",
            "美中",
            "cdt",
        ],
    ),
    (
        "America/Denver",
        "Denver",
        &["丹佛", "mountain time", "mst", "mdt"],
    ),
    (
        "America/Los_Angeles",
        "Los Angeles",
        &[
            "洛杉矶",
            "san francisco",
            "旧金山",
            "seattle",
            "西雅图",
            "silicon valley",
            "硅谷",
            "pacific time",
            "美西",
            "美国西部",
            "pst",
            "pdt",
            "california",
            "加州",
        ],
    ),
    (
        "America/Toronto",
        "Toronto",
        &[
            "多伦多",
            "canada",
            "加拿大",
            "montreal",
            "蒙特利尔",
            "ottawa",
        ],
    ),
    ("America/Vancouver", "Vancouver", &["温哥华"]),
    (
        "America/Mexico_City",
        "Mexico City",
        &["墨西哥城", "mexico", "墨西哥"],
    ),
    (
        "America/Sao_Paulo",
        "São Paulo",
        &["圣保罗", "brazil", "巴西", "rio de janeiro", "里约"],
    ),
    (
        "America/Argentina/Buenos_Aires",
        "Buenos Aires",
        &["布宜诺斯艾利斯", "argentina", "阿根廷"],
    ),
    (
        "America/Santiago",
        "Santiago",
        &["圣地亚哥", "chile", "智利"],
    ),
    (
        "America/Bogota",
        "Bogotá",
        &["波哥大", "colombia", "哥伦比亚"],
    ),
    ("America/Lima", "Lima", &["利马", "peru", "秘鲁"]),
    (
        "Pacific/Honolulu",
        "Honolulu",
        &["檀香山", "hawaii", "夏威夷"],
    ),
    (
        "Pacific/Auckland",
        "Auckland",
        &["奥克兰", "new zealand", "新西兰", "wellington", "惠灵顿"],
    ),
    (
        "Australia/Sydney",
        "Sydney",
        &[
            "悉尼",
            "australia",
            "澳大利亚",
            "澳洲",
            "melbourne",
            "墨尔本",
            "canberra",
            "堪培拉",
            "aest",
            "aedt",
        ],
    ),
    ("Australia/Perth", "Perth", &["珀斯"]),
    ("Australia/Brisbane", "Brisbane", &["布里斯班"]),
    (
        "UTC",
        "UTC",
        &[
            "utc",
            "gmt+0",
            "universal time",
            "协调世界时",
            "世界时",
            "格林尼治",
        ],
    ),
];

static TABLE: LazyLock<ZoneTable> = LazyLock::new(|| {
    let mut by_key = HashMap::new();
    let mut zh_name = HashMap::new();
    for &(zone, name, aliases) in ZONE_ROWS {
        by_key.insert(name.to_lowercase(), (zone, name));
        for &alias in aliases {
            by_key.insert(alias.to_lowercase(), (zone, name));
            if !zh_name.contains_key(zone) && contains_cjk(alias) {
                zh_name.insert(zone, alias);
            }
        }
    }
    let mut keys: Vec<String> = by_key.keys().cloned().collect();
    // Longest keys first (by char count, tie-breaking deterministically by key).
    keys.sort_by(|a, b| {
        b.chars()
            .count()
            .cmp(&a.chars().count())
            .then_with(|| a.cmp(b))
    });
    let keys_by_len_desc = keys
        .into_iter()
        .map(|k| {
            let ascii = is_ascii_key(&k);
            (k, ascii)
        })
        .collect();
    ZoneTable {
        by_key,
        keys_by_len_desc,
        zh_name,
    }
});

/// Find an ASCII key inside `haystack` ensuring neither preceding nor following byte is `a..=z`.
fn find_ascii_bounded(haystack: &str, needle: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let mut start = 0;
    while let Some(rel) = haystack[start..].find(needle) {
        let pos = start + rel;
        let end = pos + needle.len();
        let before_ok = pos == 0 || !bytes[pos - 1].is_ascii_lowercase();
        let after_ok = end == bytes.len() || !bytes[end].is_ascii_lowercase();
        if before_ok && after_ok {
            return Some(pos);
        }
        // Advance by one char to avoid infinite loops and maintain UTF-8 boundaries.
        let step = haystack[pos..].chars().next().map_or(1, char::len_utf8);
        start = pos + step;
    }
    None
}

/// Extract up to `limit` `(iana_zone, display_name)` pairs named in `query`, in query order.
pub fn zones_in(query: &str, limit: usize) -> Vec<(String, String)> {
    let mut hits: Vec<(usize, (String, String))> = Vec::new();
    let mut taken: HashSet<String> = HashSet::new();

    for cap in IANA_RE.captures_iter(query) {
        let m = cap.get(1).expect("group 1");
        let zone = m.as_str();
        if TimeZone::get(zone).is_err() {
            continue;
        }
        if taken.insert(zone.to_owned()) {
            let display = zone.rsplit('/').next().unwrap_or(zone).replace('_', " ");
            hits.push((m.start(), (zone.to_owned(), display)));
        }
    }

    let mut lowered = query.to_lowercase();
    for (key, ascii) in &TABLE.keys_by_len_desc {
        let found = if *ascii {
            find_ascii_bounded(&lowered, key)
        } else {
            lowered.find(key.as_str())
        };
        let Some(at) = found else {
            continue;
        };
        let &(zone, name) = &TABLE.by_key[key];
        if !taken.insert(zone.to_owned()) {
            continue;
        }
        hits.push((at, (zone.to_owned(), name.to_owned())));
        // Mask the matched byte span with ASCII spaces of equal byte length so indices stay stable.
        let mut masked = String::with_capacity(lowered.len());
        masked.push_str(&lowered[..at]);
        for _ in 0..key.len() {
            masked.push(' ');
        }
        masked.push_str(&lowered[at + key.len()..]);
        lowered = masked;
    }

    hits.sort_by_key(|(pos, _)| *pos);
    hits.into_iter().map(|(_, pair)| pair).take(limit).collect()
}

fn format_utc_offset(secs: i32) -> String {
    let sign = if secs < 0 { '-' } else { '+' };
    let abs = secs.unsigned_abs();
    let hours = abs / 3600;
    let mins = (abs % 3600) / 60;
    format!("UTC{sign}{hours:02}:{mins:02}")
}

fn weekday_en(wd: jiff::civil::Weekday) -> &'static str {
    match wd {
        jiff::civil::Weekday::Monday => "Monday",
        jiff::civil::Weekday::Tuesday => "Tuesday",
        jiff::civil::Weekday::Wednesday => "Wednesday",
        jiff::civil::Weekday::Thursday => "Thursday",
        jiff::civil::Weekday::Friday => "Friday",
        jiff::civil::Weekday::Saturday => "Saturday",
        jiff::civil::Weekday::Sunday => "Sunday",
    }
}

fn weekday_zh(wd: jiff::civil::Weekday) -> &'static str {
    match wd {
        jiff::civil::Weekday::Monday => "星期一",
        jiff::civil::Weekday::Tuesday => "星期二",
        jiff::civil::Weekday::Wednesday => "星期三",
        jiff::civil::Weekday::Thursday => "星期四",
        jiff::civil::Weekday::Friday => "星期五",
        jiff::civil::Weekday::Saturday => "星期六",
        jiff::civil::Weekday::Sunday => "星期日",
    }
}

/// Pure local clock engine (no network calls).
pub struct WorldClockEngine {
    decl: SourceDecl,
}

impl WorldClockEngine {
    pub fn new() -> Self {
        let now_op = OpDecl::new(
            "now",
            "Return structured current date/time records for a named city or IANA timezone.",
            ReturnKind::Records,
        )
        .with_param(Param::new(
            "query",
            ParamType::String,
            "City, country, or IANA timezone (defaults to UTC or Beijing for Chinese)",
        ))
        .with_example(r#"source.call("worldclock.now", { query: "Tokyo and New York" })"#);

        let mut decl = SourceDecl::new(
            "worldclock",
            "World Clock",
            "The current date and time in a named city or zone, from the system clock.",
        )
        .with_scopes(&["reference"])
        .with_caps(&[Capability::Search, Capability::Lookup])
        .with_op(
            search_op(
                "Get current local date, time, UTC offset, and DST status for named cities or zones.",
                10,
                r#"source.call("worldclock.search", { query: "what time is it in Tokyo and New York" })"#,
            )
            .with_probe(fsm_core::ProbeSpec::new(json!({"query": "Tokyo"}), 1)),
        )
        .with_op(now_op);
        decl.aliases = vec![
            "clock".to_owned(),
            "time".to_owned(),
            "timezone".to_owned(),
            "时区".to_owned(),
            "北京时间".to_owned(),
        ];
        decl.home_url = Some("https://time.is".to_owned());
        decl.auth = AuthMode::None;
        decl.transport = TransportProfile::Local;
        decl.pool_role = PoolRole::DirectClaimant;
        // SPEC §5: worldclock TTL = 0 (never cache current time).
        decl.limits = SourceLimits::new(100.0, 100, 16, 1000, 0);
        Self { decl }
    }

    /// Format `(zone, display_name)` pairs at a specific UTC `now` timestamp.
    pub fn format_zones(
        now: Timestamp,
        zones: &[(String, String)],
        zh: bool,
    ) -> (Vec<Hit>, Vec<Value>) {
        let utc = now.to_zoned_datetime(TimeZone::UTC);
        let utc_str = format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            utc.year(),
            utc.month(),
            utc.day(),
            utc.hour(),
            utc.minute(),
            utc.second()
        );
        let mut hits = Vec::new();
        let mut records = Vec::new();
        for (zone, name) in zones {
            let Ok(tz) = TimeZone::get(zone) else {
                continue;
            };
            let local = now.to_zoned_datetime(tz.clone());
            let info = tz.to_offset_info(now);
            let offset_str = format_utc_offset(info.offset().seconds());
            let dst = info.dst().is_dst();
            let date_iso = local.date().to_string();
            let hm = format!("{:02}:{:02}", local.hour(), local.minute());
            let hms = format!(
                "{:02}:{:02}:{:02}",
                local.hour(),
                local.minute(),
                local.second()
            );

            let (title, snippet) = if zh {
                let wd = weekday_zh(local.weekday());
                let zh_city = TABLE
                    .zh_name
                    .get(zone.as_str())
                    .copied()
                    .unwrap_or(name.as_str());
                let title = format!("{zh_city}现在时间：{date_iso} {hm}（{zone}）");
                let snippet = format!(
                    "{:04}年{:02}月{:02}日 {hms} {wd}，{offset_str}，{} · 协调世界时 {utc_str} · 来自服务器时钟和 IANA 时区数据库，不是网页",
                    local.year(),
                    local.month(),
                    local.day(),
                    if dst {
                        "夏令时生效"
                    } else {
                        "无夏令时"
                    }
                );
                (title, snippet)
            } else {
                let wd = weekday_en(local.weekday());
                let title = format!("Current time in {name}: {date_iso} {hm} ({zone})");
                let snippet = format!(
                    "{date_iso} {hms} {wd}, {offset_str}, {} · UTC {utc_str} · from the server clock and the IANA zone database, not a web page",
                    if dst {
                        "daylight saving in force"
                    } else {
                        "no daylight saving"
                    }
                );
                (title, snippet)
            };

            let url = format!("https://time.is/{}", name.replace(' ', "_"));
            hits.push(make_hit(
                "worldclock",
                title,
                url.clone(),
                snippet,
                Some(date_iso.clone()),
                Some("clock"),
                HitKind::Record,
            ));
            records.push(json!({
                "zone": zone,
                "name": name,
                "date": date_iso,
                "time": hms,
                "weekday": if zh { weekday_zh(local.weekday()) } else { weekday_en(local.weekday()) },
                "utcOffset": offset_str,
                "dst": dst,
                "utc": utc_str,
                "url": url,
            }));
        }
        (hits, records)
    }
}

impl Default for WorldClockEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Source for WorldClockEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn claims(&self, query: &str, _lang: Option<&str>) -> f32 {
        if TIME_ASK_RE.is_match(query) {
            1.0
        } else {
            0.0
        }
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        if cx.cancel.is_cancelled() {
            return Err(FsmError::Timeout(0));
        }
        let query = match op {
            "search" => params
                .get("query")
                .and_then(Value::as_str)
                .ok_or_else(|| FsmError::invalid("missing required string parameter 'query'"))?,
            "now" => params
                .get("query")
                .or_else(|| params.get("zone"))
                .and_then(Value::as_str)
                .unwrap_or(""),
            _ => {
                return Err(FsmError::invalid(format!(
                    "source 'worldclock' has no op '{op}'; call 'worldclock.search' or 'worldclock.now'"
                )));
            }
        };

        let limit = parse_limit(&params, 3).min(3);
        let zh = query_is_chinese(query);
        let mut zones = zones_in(query, 3);
        if zones.is_empty() {
            zones = if zh {
                vec![("Asia/Shanghai".to_owned(), "Beijing".to_owned())]
            } else {
                vec![("UTC".to_owned(), "UTC".to_owned())]
            };
        }
        zones.truncate(limit);

        let now = Timestamp::now();
        let (hits, records) = Self::format_zones(now, &zones, zh);
        match op {
            "now" => Ok(SrcOutput::Records {
                items: records,
                hits: Some(hits),
                next_cursor: None,
            }),
            _ => Ok(SrcOutput::Hits(hits)),
        }
    }
}
