//! CoinGecko: current spot price, 24h change, and market cap of a cryptocurrency.
//!
//! Port of `src/search_mcp/engines/coingecko.py`.

use crate::facts::{
    SNIPPET_CAP, STOP_WORDS, TOKEN_RE, clip, clip_query, find_ascii_letter_bounded,
    fmt_comma_fixed, fmt_g, query_is_chinese, utc_minute_epoch,
};
use crate::http::{
    CallBudget, build_source_decl, encode_query_plus, get_json, hits_output, make_hit,
    parse_search_params,
};
use fsm_core::{
    Availability, Config, FsmError, Hit, PoolRole, ProbeSpec, Source, SourceDecl, SourceDeps,
    SourceLimits, SrcCx, SrcOutput,
};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::LazyLock;

const API_URL: &str = "https://api.coingecko.com/api/v3";

const RAW_COINS: &[(&str, &str)] = &[
    ("btc", "bitcoin"),
    ("bitcoin", "bitcoin"),
    ("比特币", "bitcoin"),
    ("eth", "ethereum"),
    ("ethereum", "ethereum"),
    ("以太坊", "ethereum"),
    ("以太币", "ethereum"),
    ("sol", "solana"),
    ("solana", "solana"),
    ("bnb", "binancecoin"),
    ("binance coin", "binancecoin"),
    ("币安币", "binancecoin"),
    ("xrp", "ripple"),
    ("ripple", "ripple"),
    ("瑞波", "ripple"),
    ("doge", "dogecoin"),
    ("dogecoin", "dogecoin"),
    ("狗狗币", "dogecoin"),
    ("ada", "cardano"),
    ("cardano", "cardano"),
    ("usdt", "tether"),
    ("tether", "tether"),
    ("泰达币", "tether"),
    ("usdc", "usd-coin"),
    ("ton", "the-open-network"),
    ("toncoin", "the-open-network"),
    ("trx", "tron"),
    ("tron", "tron"),
    ("波场", "tron"),
    ("dot", "polkadot"),
    ("polkadot", "polkadot"),
    ("avax", "avalanche-2"),
    ("avalanche", "avalanche-2"),
    ("link", "chainlink"),
    ("chainlink", "chainlink"),
    ("ltc", "litecoin"),
    ("litecoin", "litecoin"),
    ("莱特币", "litecoin"),
    ("matic", "matic-network"),
    ("pol", "matic-network"),
    ("polygon", "matic-network"),
    ("shib", "shiba-inu"),
    ("shiba", "shiba-inu"),
    ("bch", "bitcoin-cash"),
    ("bitcoin cash", "bitcoin-cash"),
    ("xlm", "stellar"),
    ("stellar", "stellar"),
    ("atom", "cosmos"),
    ("cosmos", "cosmos"),
    ("uni", "uniswap"),
    ("uniswap", "uniswap"),
    ("etc", "ethereum-classic"),
    ("xmr", "monero"),
    ("monero", "monero"),
    ("门罗币", "monero"),
    ("near", "near"),
    ("apt", "aptos"),
    ("aptos", "aptos"),
    ("sui", "sui"),
    ("arb", "arbitrum"),
    ("arbitrum", "arbitrum"),
    ("op", "optimism"),
    ("optimism", "optimism"),
    ("pepe", "pepe"),
    ("wif", "dogwifcoin"),
    ("hype", "hyperliquid"),
    ("hyperliquid", "hyperliquid"),
];

static COINS_BY_LEN: LazyLock<Vec<(&'static str, &'static str)>> = LazyLock::new(|| {
    let mut v = RAW_COINS.to_vec();
    v.sort_by_key(|a| std::cmp::Reverse(a.0.chars().count()));
    v
});

static LABELS: LazyLock<HashMap<&'static str, (&'static str, &'static str)>> =
    LazyLock::new(|| {
        let mut map: HashMap<&'static str, (&'static str, &'static str)> = HashMap::new();
        for &(key, id) in RAW_COINS {
            if !key.is_ascii() {
                continue;
            }
            let entry = map.entry(id).or_insert(("", ""));
            if entry.1.is_empty() || key.len() < entry.1.len() {
                entry.1 = key;
            }
            if entry.0.is_empty() || key.len() > entry.0.len() {
                entry.0 = key;
            }
        }
        map
    });

static CRYPTO_ASK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(crypto|cryptocurrency|coin|token|btc|eth|sol|bnb|xrp|doge|usdt|market cap|bitcoin|ethereum|solana|dogecoin|altcoin)\b|币价|加密货币|数字货币|虚拟货币|比特币|以太坊|狗狗币|币圈|市值",
    )
    .expect("crypto_ask regex")
});

static PRICE_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(price|prices|quote|rate|worth|value|today|now|live|usd|cny|dollars?)\b|价格|多少钱|行情|报价|今日|现在|美元|人民币",
    )
    .expect("price_words regex")
});

/// Replace every ASCII-letter-bounded occurrence of `needle` with same-length
/// spaces in a single left-to-right pass (strictly O(N)).
fn blank_ascii_letter_bounded(haystack: &mut String, needle: &str) {
    if needle.is_empty() {
        return;
    }
    let spaces = " ".repeat(needle.len());
    let mut pos = 0usize;
    while pos < haystack.len() {
        let Some(rel) = haystack[pos..].find(needle) else {
            break;
        };
        let at = pos + rel;
        let end = at + needle.len();
        let bytes = haystack.as_bytes();
        let before_ok = at == 0 || !bytes[at - 1].is_ascii_lowercase();
        let after_ok = end == bytes.len() || !bytes[end].is_ascii_lowercase();
        if before_ok && after_ok {
            haystack.replace_range(at..end, &spaces);
            pos = end;
        } else {
            pos = at + haystack[at..].chars().next().map_or(1, char::len_utf8);
        }
    }
}

/// CoinGecko IDs for coins named in `query`, from the built-in table only.
pub fn coins_in(query: &str, limit: usize) -> Vec<String> {
    let mut lowered = query.to_lowercase();
    let mut found: Vec<String> = Vec::new();
    for &(key, id) in COINS_BY_LEN.iter() {
        let matched = if key.is_ascii() {
            find_ascii_letter_bounded(&lowered, key).is_some()
        } else {
            lowered.contains(key)
        };
        if matched && !found.iter().any(|x| x == id) {
            found.push(id.to_owned());
            if key.is_ascii() {
                blank_ascii_letter_bounded(&mut lowered, key);
            } else {
                let spaces = " ".repeat(key.len());
                lowered = lowered.replace(key, &spaces);
            }
        }
    }
    found.truncate(limit);
    found
}

fn unknown_coin_words(query: &str) -> Vec<String> {
    let stripped_crypto = CRYPTO_ASK.replace_all(query, " ");
    let text = PRICE_WORDS.replace_all(&stripped_crypto, " ");
    TOKEN_RE
        .find_iter(&text)
        .map(|m| m.as_str().to_ascii_lowercase())
        .filter(|w| !STOP_WORDS.contains(w.as_str()) && !w.bytes().all(|b| b.is_ascii_digit()))
        .take(2)
        .collect()
}

fn price_url(ids: &[String]) -> String {
    format!(
        "{API_URL}/simple/price?ids={}&vs_currencies=usd,cny&include_24hr_change=true&include_market_cap=true&include_last_updated_at=true",
        ids.join(",")
    )
}

fn money(value: f64) -> String {
    let abs = value.abs();
    if abs >= 1e9 {
        format!("{} billion", fmt_comma_fixed(value / 1e9, 2))
    } else if abs >= 1000.0 {
        fmt_comma_fixed(value, 0)
    } else if abs >= 1.0 {
        fmt_comma_fixed(value, 2)
    } else {
        fmt_g(value, 6)
    }
}

fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut cap_next = true;
    for ch in s.chars() {
        if ch.is_alphabetic() {
            if cap_next {
                for u in ch.to_uppercase() {
                    out.push(u);
                }
                cap_next = false;
            } else {
                for l in ch.to_lowercase() {
                    out.push(l);
                }
            }
        } else {
            out.push(ch);
            cap_next = true;
        }
    }
    out
}

pub struct CoinGeckoEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl CoinGeckoEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let decl = build_source_decl(
            "coingecko",
            "CoinGecko",
            "CoinGecko: current price, 24h change and market cap of a cryptocurrency.",
            &["crypto", "bitcoin", "btc", "eth", "加密货币", "币价"],
            "https://www.coingecko.com",
            &["finance"],
            PoolRole::DirectClaimant,
            SourceLimits::new(0.16, 2, 1, 5_000, 3_600),
            r#"source.call("coingecko.search", {query: "btc price"})"#,
            ProbeSpec::new(json!({"query": "btc price", "limit": 3}), 1),
        );
        Self { decl, deps }
    }

    pub fn build_url(query: &str) -> String {
        let mut ids = coins_in(query, 3);
        if ids.is_empty() {
            ids.push("bitcoin".to_owned());
        }
        price_url(&ids)
    }

    pub fn map_results(&self, payload: &Value) -> Vec<Hit> {
        let Some(obj) = payload.as_object() else {
            return Vec::new();
        };
        let names_obj = obj.get("_names").and_then(Value::as_object);
        let zh = obj.get("_zh").and_then(Value::as_bool).unwrap_or(false);

        let mut results = Vec::new();
        for (coin_id, val) in obj {
            if coin_id.starts_with('_') {
                continue;
            }
            let Some(row) = val.as_object() else {
                continue;
            };
            let Some(usd) = row.get("usd").and_then(Value::as_f64) else {
                continue;
            };
            let cny = row.get("cny").and_then(Value::as_f64);

            let (mut name, symbol) = names_obj
                .and_then(|m| m.get(coin_id))
                .and_then(Value::as_array)
                .map(|arr| {
                    (
                        arr.first().and_then(Value::as_str).unwrap_or("").to_owned(),
                        arr.get(1).and_then(Value::as_str).unwrap_or("").to_owned(),
                    )
                })
                .or_else(|| {
                    LABELS
                        .get(coin_id.as_str())
                        .map(|&(n, s)| (n.to_owned(), s.to_owned()))
                })
                .unwrap_or_else(|| (title_case(&coin_id.replace('-', " ")), String::new()));

            if name.chars().any(char::is_alphabetic)
                && name
                    .chars()
                    .filter(|c| c.is_alphabetic())
                    .all(char::is_lowercase)
            {
                name = title_case(&name);
            }
            let label = if !symbol.is_empty() {
                format!("{name} ({})", symbol.to_uppercase())
            } else {
                name.clone()
            };
            let when = row
                .get("last_updated_at")
                .and_then(Value::as_f64)
                .map(utc_minute_epoch)
                .unwrap_or_default();
            let day: String = when.chars().take(10).collect();

            let unit_label = if !symbol.is_empty() {
                symbol.to_uppercase()
            } else {
                name.clone()
            };
            let mut price_bit = format!("1 {unit_label} = {} USD", money(usd));
            if let Some(cny_val) = cny {
                price_bit.push_str(&format!(" = {} CNY", money(cny_val)));
            }
            let mut bits = vec![price_bit];
            if let Some(change) = row.get("usd_24h_change").and_then(Value::as_f64) {
                bits.push(format!("24h {change:+.2}%"));
            }
            if let Some(cap) = row.get("usd_market_cap").and_then(Value::as_f64)
                && cap != 0.0
            {
                bits.push(format!("market cap {} USD", money(cap)));
            }
            let provenance = if zh {
                if !when.is_empty() {
                    format!("CoinGecko 聚合现货价，更新于 {when}，不是交易所报价")
                } else {
                    "CoinGecko 聚合现货价，不是交易所报价".to_owned()
                }
            } else if !when.is_empty() {
                format!("CoinGecko aggregate spot price, updated {when}; not an exchange quote")
            } else {
                "CoinGecko aggregate spot price; not an exchange quote".to_owned()
            };
            bits.push(provenance);

            let title = if !when.is_empty() {
                format!("{label}: {} USD ({when})", money(usd))
            } else {
                format!("{label}: {} USD", money(usd))
            };
            results.push(make_hit(
                &self.decl.id,
                title,
                format!("https://www.coingecko.com/en/coins/{coin_id}"),
                clip(&bits.join(" · "), SNIPPET_CAP),
                (!day.is_empty()).then_some(day),
            ));
        }
        results
    }
}

#[async_trait::async_trait]
impl Source for CoinGeckoEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    fn claims(&self, query: &str, _lang: Option<&str>) -> f32 {
        let q = clip_query(query);
        if CRYPTO_ASK.is_match(q)
            && (!coins_in(q, 3).is_empty() || !unknown_coin_words(q).is_empty())
        {
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
        let budget = CallBudget::from_cx(cx, &self.decl);
        let mut ids = coins_in(&query, 3);
        let mut names_map = Map::new();
        if ids.is_empty() {
            let words = unknown_coin_words(&query);
            if words.is_empty() {
                return Ok(hits_output(Vec::new()));
            }
            let search_url = format!(
                "{API_URL}/search?query={}",
                encode_query_plus(&words.join(" "))
            );
            let found =
                get_json(&self.deps, &self.decl, cx, &budget, &search_url, &[], None).await?;
            if let Some(coins) = found
                .as_ref()
                .and_then(|v| v.get("coins"))
                .and_then(Value::as_array)
            {
                for coin in coins.iter().take(1) {
                    if let Some(id) = coin.get("id").and_then(Value::as_str) {
                        let name = coin.get("name").and_then(Value::as_str).unwrap_or("");
                        let sym = coin.get("symbol").and_then(Value::as_str).unwrap_or("");
                        ids.push(id.to_owned());
                        names_map.insert(id.to_owned(), json!([name, sym]));
                    }
                }
            }
            if ids.is_empty() {
                return Ok(hits_output(Vec::new()));
            }
        }

        let Some(prices_val) = get_json(
            &self.deps,
            &self.decl,
            cx,
            &budget,
            price_url(&ids),
            &[],
            None,
        )
        .await?
        else {
            return Ok(hits_output(Vec::new()));
        };
        let Some(prices_obj) = prices_val.as_object() else {
            return Ok(hits_output(Vec::new()));
        };
        let mut filtered = Map::new();
        for (k, v) in prices_obj {
            if ids.iter().any(|id| id == k) {
                filtered.insert(k.clone(), v.clone());
            }
        }
        filtered.insert("_names".to_owned(), Value::Object(names_map));
        filtered.insert("_zh".to_owned(), Value::Bool(query_is_chinese(&query)));
        let mut hits = self.map_results(&Value::Object(filtered));
        hits.truncate(limit);
        Ok(hits_output(hits))
    }
}
