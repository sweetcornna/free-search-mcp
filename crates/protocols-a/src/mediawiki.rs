//! MediaWiki Action API protocol adapter (`mediawiki`, A1 §E #9 + Python `wikipedia.py`/`wikimedia.py`).

use crate::common::{
    PATH_SEGMENT_SET, clip, collapse_ws, dialect_opt_bool, dialect_opt_i64, dialect_opt_str,
    hit_to_value, normalize_date, opt_int, opt_limit, opt_str, params_map, parse_json,
    reject_unknown_dialect_keys, req_str, send_checked, strip_tags, strip_tags_joined,
    trim_trailing_slash, validate_base_url,
};
use fsm_core::{
    FsmError, Hit, HttpRequest, OpDecl, Param, ParamType, ProtocolAdapter, ProtocolOutput,
    ReturnKind, SourceDeps, SrcCx,
};
use percent_encoding::utf8_percent_encode;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use url::Url;

const KIND: &str = "mediawiki";
const DIALECT_KEYS: &[&str] = &[
    "api_path",
    "dynamic_lang_host",
    "default_namespace",
    "default_prop",
];
const DEFAULT_API_PATH: &str = "/w/api.php";

pub struct MediaWikiAdapter {
    deps: SourceDeps,
    ops: Vec<OpDecl>,
}

impl MediaWikiAdapter {
    pub fn new(deps: SourceDeps) -> Self {
        let ops = vec![
            OpDecl::new(
                "search",
                "Search a MediaWiki wiki (articles, or generator+imageinfo media search for Wikimedia Commons).",
                ReturnKind::Hits,
            )
            .with_param(Param::new("query", ParamType::String, "Search query").required())
            .with_param(
                Param::new("limit", ParamType::Int, "Maximum results to return")
                    .with_default(json!(10))
                    .with_range(1.0, 50.0),
            )
            .with_param(
                Param::new(
                    "namespace",
                    ParamType::Int,
                    "Optional MediaWiki namespace ID (e.g. 6 for File: on Wikimedia Commons)",
                )
                .with_range(0.0, 10_000.0),
            )
            .with_param(Param::new(
                "prop",
                ParamType::String,
                "Optional generator prop (e.g. imageinfo for Wikimedia Commons media search)",
            ))
            .with_param(Param::new(
                "lang",
                ParamType::String,
                "Optional language subdomain override for dynamic-host wikis (e.g. en, zh, fr)",
            )),
            OpDecl::new(
                "page",
                "Fetch plain-text extract and canonical URL for a MediaWiki page title.",
                ReturnKind::Records,
            )
            .with_param(
                Param::new("title", ParamType::String, "Exact MediaWiki page title").required(),
            )
            .with_param(Param::new(
                "lang",
                ParamType::String,
                "Optional language subdomain override for dynamic-host wikis",
            )),
        ];
        Self { deps, ops }
    }

    async fn call_search(
        &self,
        raw_base_url: &str,
        params: &Map<String, Value>,
        cx: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError> {
        let query = req_str(params, "query")?;
        let limit = opt_limit(params, "limit", 10, 1, 50)?;
        let param_ns = opt_int(params, "namespace", 0, 10_000)?;
        let param_prop = opt_str(params, "prop")?;
        let param_lang = opt_str(params, "lang")?;

        let dialect_ns = dialect_opt_i64(KIND, &cx.dialect, "default_namespace", 0, 10_000)?;
        let dialect_prop = dialect_opt_str(KIND, &cx.dialect, "default_prop")?;

        let effective_ns = param_ns.or(dialect_ns);
        let effective_prop = param_prop.or(dialect_prop);

        let (endpoint, parsed_url) = resolve_endpoint(raw_base_url, param_lang.as_deref(), cx)?;

        let mut req = HttpRequest::get(&endpoint)
            .query_param("action", "query")
            .header("Accept", "application/json");

        let using_generator = effective_prop.is_some();
        if let Some(prop) = &effective_prop {
            req = req
                .query_param("generator", "search")
                .query_param("gsrsearch", &query)
                .query_param("gsrlimit", limit.to_string());
            if let Some(ns) = effective_ns {
                req = req.query_param("gsrnamespace", ns.to_string());
            }
            req = req.query_param("prop", prop);
            if prop.to_ascii_lowercase().contains("imageinfo") {
                req = req.query_param("iiprop", "url|extmetadata|size|mime|timestamp");
            }
            req = req.query_param("format", "json");
        } else {
            req = req
                .query_param("list", "search")
                .query_param("srsearch", &query)
                .query_param("srlimit", limit.to_string());
            if let Some(ns) = effective_ns {
                req = req.query_param("srnamespace", ns.to_string());
            }
            req = req.query_param("format", "json");
        }

        let resp = send_checked(&self.deps, req, cx, KIND).await?;
        let val = parse_json(&resp, KIND)?;
        check_mw_error(&val, resp.status)?;

        let expect_imageinfo = effective_prop
            .as_deref()
            .is_some_and(|p| p.to_ascii_lowercase().contains("imageinfo"));
        let (hits, next_cursor) = if using_generator {
            parse_generator_pages(&val, &parsed_url, limit, expect_imageinfo)
        } else {
            parse_list_search(&val, &parsed_url, limit)
        };

        let mut out = ProtocolOutput::new(KIND, raw_base_url, "search");
        out.items = hits.into_iter().map(hit_to_value).collect();
        out.next_cursor = next_cursor;
        Ok(out)
    }

    async fn call_page(
        &self,
        raw_base_url: &str,
        params: &Map<String, Value>,
        cx: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError> {
        let title = req_str(params, "title")?;
        let param_lang = opt_str(params, "lang")?;
        let (endpoint, parsed_url) = resolve_endpoint(raw_base_url, param_lang.as_deref(), cx)?;

        let req = HttpRequest::get(&endpoint)
            .query_param("action", "query")
            .query_param("prop", "extracts|info")
            .query_param("inprop", "url")
            .query_param("explaintext", "1")
            .query_param("titles", &title)
            .query_param("format", "json")
            .header("Accept", "application/json");

        let resp = send_checked(&self.deps, req, cx, KIND).await?;
        let val = parse_json(&resp, KIND)?;
        check_mw_error(&val, resp.status)?;

        let mut items = Vec::new();
        if let Some(pages) = val.pointer("/query/pages").and_then(Value::as_object) {
            for page in pages.values() {
                if page.get("missing").is_some() {
                    continue;
                }
                let Some(page_title) = page
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                else {
                    continue;
                };
                let url = page
                    .get("fullurl")
                    .and_then(Value::as_str)
                    .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
                    .map(str::to_owned)
                    .unwrap_or_else(|| wiki_article_url(&parsed_url, page_title));
                let extract = page
                    .get("extract")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_owned();

                let mut rec = Map::new();
                if let Some(pid) = page.get("pageid").and_then(Value::as_i64) {
                    rec.insert("pageid".into(), json!(pid));
                }
                rec.insert("title".into(), Value::String(page_title.to_owned()));
                rec.insert("url".into(), Value::String(url));
                rec.insert("extract".into(), Value::String(extract));
                if let Some(touched) = page
                    .get("touched")
                    .and_then(Value::as_str)
                    .and_then(normalize_date)
                {
                    rec.insert("lastEdited".into(), Value::String(touched));
                }
                if let Some(len) = page.get("length").and_then(Value::as_u64) {
                    rec.insert("length".into(), json!(len));
                }
                items.push(Value::Object(rec));
            }
        }

        let mut out = ProtocolOutput::new(KIND, raw_base_url, "page");
        out.items = items;
        Ok(out)
    }
}

/// Resolve dynamic `{lang_sub}` host (A1 §E #9 & Python `wikipedia.py`) and
/// preserve any `base_url` ending in `.php`.
pub(crate) fn resolve_endpoint(
    raw_base_url: &str,
    param_lang: Option<&str>,
    cx: &SrcCx,
) -> Result<(String, Url), FsmError> {
    let dynamic_lang = dialect_opt_bool(KIND, &cx.dialect, "dynamic_lang_host")?.unwrap_or(false);
    let api_path = dialect_opt_str(KIND, &cx.dialect, "api_path")?
        .unwrap_or_else(|| DEFAULT_API_PATH.to_owned());

    let mut base = raw_base_url.trim().to_owned();
    if dynamic_lang || base.contains("{lang_sub}") {
        let lang_sub = resolve_lang_subtag(param_lang, cx.lang.as_deref(), &cx.region);
        base = base.replace("{lang_sub}", &lang_sub);
    }

    let endpoint = if base.to_ascii_lowercase().ends_with(".php") {
        base
    } else {
        let path = if api_path.starts_with('/') {
            api_path
        } else {
            format!("/{api_path}")
        };
        format!("{}{}", trim_trailing_slash(&base), path)
    };

    let parsed = validate_base_url(&endpoint)?;
    Ok((endpoint, parsed))
}

/// Priority: `params.lang` -> `cx.lang` -> `cx.region` language subtag -> `"en"`.
/// Strictly guards hostname safety: only 2..=3 ASCII letters are accepted.
pub(crate) fn resolve_lang_subtag(
    param_lang: Option<&str>,
    cx_lang: Option<&str>,
    cx_region: &str,
) -> String {
    if let Some(l) = param_lang.and_then(extract_primary_lang) {
        return l;
    }
    if let Some(l) = cx_lang.and_then(extract_primary_lang) {
        return l;
    }
    // DDG-style region: "us-en" -> "en", "cn-zh" -> "zh", "wt-wt" -> fallback "en".
    if let Some((_, rhs)) = cx_region.trim().split_once('-')
        && let Some(l) = valid_lang_subtag(rhs)
    {
        return l;
    }
    "en".to_owned()
}

fn extract_primary_lang(raw: &str) -> Option<String> {
    let primary = raw.trim().split(['-', '_']).next()?;
    valid_lang_subtag(primary)
}

fn valid_lang_subtag(raw: &str) -> Option<String> {
    let lower = raw.trim().to_ascii_lowercase();
    if (2..=3).contains(&lower.len())
        && lower.bytes().all(|b| b.is_ascii_lowercase())
        && lower != "wt"
    {
        Some(lower)
    } else {
        None
    }
}

fn check_mw_error(val: &Value, status: u16) -> Result<(), FsmError> {
    if let Some(err) = val.get("error").and_then(Value::as_object) {
        let code = err.get("code").and_then(Value::as_str).unwrap_or("error");
        let info = err.get("info").and_then(Value::as_str).unwrap_or("");
        return Err(FsmError::upstream(
            KIND,
            Some(status),
            if info.is_empty() {
                format!("MediaWiki API error: {code}")
            } else {
                format!("MediaWiki API error ({code}): {info}")
            },
        ));
    }
    Ok(())
}

/// Build an article URL from the resolved MediaWiki API endpoint:
/// - Standard `/w/api.php` layout (Wikimedia, OSM Wiki) -> `{origin}/wiki/{title}`.
/// - Any other layout (e.g. ArchWiki `/api.php`, subpath `/custom/api.php`) ->
///   `{origin}{script_dir}index.php?title={title}` (since `index.php` always sits next to `api.php`).
pub(crate) fn wiki_article_url(endpoint_url: &Url, title: &str) -> String {
    let origin = endpoint_url.origin().ascii_serialization();
    let underscored = title.replace(' ', "_");
    let encoded = utf8_percent_encode(&underscored, PATH_SEGMENT_SET).to_string();
    let path = endpoint_url.path();
    if path.eq_ignore_ascii_case("/w/api.php") {
        return format!("{origin}/wiki/{encoded}");
    }
    let script_dir = match path.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/"),
        None => "/".to_owned(),
    };
    format!("{origin}{script_dir}index.php?title={encoded}")
}

fn parse_list_search(val: &Value, endpoint_url: &Url, limit: usize) -> (Vec<Hit>, Option<String>) {
    let mut hits = Vec::new();
    if let Some(arr) = val.pointer("/query/search").and_then(Value::as_array) {
        for item in arr {
            if hits.len() >= limit {
                break;
            }
            let Some(title) = item
                .get("title")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let snippet = item
                .get("snippet")
                .and_then(Value::as_str)
                .map(|s| clip(&strip_tags_joined(s), 400))
                .unwrap_or_default();
            let last_edited = item
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(normalize_date);

            let mut hit = Hit::new(title, wiki_article_url(endpoint_url, title));
            hit.snippet = snippet;
            if let Some(edited) = last_edited {
                hit.extra = Some(json!({ "lastEdited": edited }));
            }
            hits.push(hit);
        }
    }

    let next_cursor = val.pointer("/continue/sroffset").and_then(|v| {
        v.as_i64()
            .map(|n| n.to_string())
            .or_else(|| v.as_str().map(str::to_owned))
    });

    (hits, next_cursor)
}

fn parse_generator_pages(
    val: &Value,
    endpoint_url: &Url,
    limit: usize,
    expect_imageinfo: bool,
) -> (Vec<Hit>, Option<String>) {
    let mut pages: Vec<&Value> = val
        .pointer("/query/pages")
        .and_then(Value::as_object)
        .map(|m| m.values().collect())
        .unwrap_or_default();

    // Preserve MediaWiki search rank order via `index` when present.
    pages.sort_by_key(|p| p.get("index").and_then(Value::as_u64).unwrap_or(u64::MAX));

    let mut hits = Vec::new();
    for page in pages {
        if hits.len() >= limit {
            break;
        }
        let Some(raw_title) = page
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };

        if let Some(info) = page
            .get("imageinfo")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(Value::as_object)
        {
            // Port of Python wikimedia.py: File: prefix stripped, direct image URL, attribution snippet.
            let title = raw_title.strip_prefix("File:").unwrap_or(raw_title).trim();
            if title.is_empty() {
                continue;
            }
            let Some(url) = info
                .get("url")
                .and_then(Value::as_str)
                .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
            else {
                continue;
            };
            let date = info
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(normalize_date);

            let mut hit = Hit::new(clip(title, 300), url);
            hit.snippet = build_wikimedia_snippet(info);
            if let Some(d) = date {
                hit.date = Some(d);
                hit.date_source = Some("api".to_owned());
            }
            hits.push(hit);
        } else if !expect_imageinfo {
            let url = page
                .get("fullurl")
                .and_then(Value::as_str)
                .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
                .map(str::to_owned)
                .unwrap_or_else(|| wiki_article_url(endpoint_url, raw_title));
            let snippet = page
                .get("extract")
                .and_then(Value::as_str)
                .map(|s| clip(&strip_tags(s), 400))
                .unwrap_or_default();
            let mut hit = Hit::new(raw_title, url);
            hit.snippet = snippet;
            hits.push(hit);
        }
    }

    let next_cursor = val.pointer("/continue/gsroffset").and_then(|v| {
        v.as_i64()
            .map(|n| n.to_string())
            .or_else(|| v.as_str().map(str::to_owned))
    });

    (hits, next_cursor)
}

fn extmeta_value(extmetadata: Option<&Map<String, Value>>, key: &str) -> String {
    extmetadata
        .and_then(|m| m.get(key))
        .and_then(|b| b.get("value"))
        .and_then(Value::as_str)
        .map(strip_tags)
        .unwrap_or_default()
}

/// Port of Python `wikimedia.py` `_snippet`: `"License · by Artist · W×Hpx — ImageDescription"`.
fn build_wikimedia_snippet(info: &Map<String, Value>) -> String {
    let ext = info.get("extmetadata").and_then(Value::as_object);
    let mut bits: Vec<String> = Vec::new();
    let license = extmeta_value(ext, "LicenseShortName");
    if !license.is_empty() {
        bits.push(license);
    }
    let artist = extmeta_value(ext, "Artist");
    if !artist.is_empty() {
        bits.push(format!("by {artist}"));
    }
    if let (Some(w), Some(h)) = (
        info.get("width").and_then(Value::as_u64),
        info.get("height").and_then(Value::as_u64),
    ) {
        bits.push(format!("{w}\u{00D7}{h}px"));
    }
    let description = extmeta_value(ext, "ImageDescription");
    let head = bits.join(" \u{00B7} ");
    let combined = match (head.is_empty(), description.is_empty()) {
        (false, false) => format!("{head} \u{2014} {description}"),
        (true, false) => description,
        (false, true) => head,
        (true, true) => String::new(),
    };
    clip(&collapse_ws(&combined), 400)
}

#[async_trait::async_trait]
impl ProtocolAdapter for MediaWikiAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn ops(&self) -> &[OpDecl] {
        &self.ops
    }

    fn validate_dialect(&self, dialect: &BTreeMap<String, Value>) -> Result<(), FsmError> {
        reject_unknown_dialect_keys(KIND, dialect, DIALECT_KEYS)?;
        let _ = dialect_opt_str(KIND, dialect, "api_path")?;
        let _ = dialect_opt_bool(KIND, dialect, "dynamic_lang_host")?;
        let _ = dialect_opt_i64(KIND, dialect, "default_namespace", 0, 10_000)?;
        let _ = dialect_opt_str(KIND, dialect, "default_prop")?;
        Ok(())
    }

    async fn call(
        &self,
        base_url: &str,
        op: &str,
        params: Value,
        cx: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError> {
        self.validate_dialect(&cx.dialect)?;
        let map = params_map(&params)?;
        match op {
            "search" => self.call_search(base_url, map, cx).await,
            "page" => self.call_page(base_url, map, cx).await,
            other => Err(FsmError::invalid(format!(
                "unknown op {other:?} for protocol {KIND}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_subtag_priority_and_host_injection_guard() {
        assert_eq!(resolve_lang_subtag(Some("FR"), Some("de"), "us-en"), "fr");
        assert_eq!(resolve_lang_subtag(None, Some("zh-CN"), "us-en"), "zh");
        assert_eq!(resolve_lang_subtag(None, None, "cn-zh"), "zh");
        assert_eq!(resolve_lang_subtag(None, None, "us-en"), "en");
        assert_eq!(resolve_lang_subtag(None, None, "wt-wt"), "en");
        assert_eq!(
            resolve_lang_subtag(Some("en.evil.com"), Some("../jp"), "bad"),
            "en"
        );
        assert_eq!(resolve_lang_subtag(Some("12"), Some("a"), "xx-1234"), "en");
    }

    #[test]
    fn endpoint_resolution_handles_php_suffix_and_dynamic_host() {
        let mut cx = SrcCx::default();
        cx.region = "jp-ja".into();
        cx.dialect.insert("dynamic_lang_host".into(), json!(true));

        let (ep, _) = resolve_endpoint("https://{lang_sub}.wikipedia.org", None, &cx).unwrap();
        assert_eq!(ep, "https://ja.wikipedia.org/w/api.php");

        let (ep_arch, _) = resolve_endpoint(
            "https://wiki.archlinux.org/api.php",
            None,
            &SrcCx::default(),
        )
        .unwrap();
        assert_eq!(ep_arch, "https://wiki.archlinux.org/api.php");

        let mut cx_custom = SrcCx::default();
        cx_custom
            .dialect
            .insert("api_path".into(), json!("/custom/api.php"));
        let (ep_c, _) = resolve_endpoint("https://wiki.example.org/", None, &cx_custom).unwrap();
        assert_eq!(ep_c, "https://wiki.example.org/custom/api.php");
    }

    #[test]
    fn article_url_uses_wiki_for_w_api_and_index_php_otherwise() {
        let wp = Url::parse("https://en.wikipedia.org/w/api.php").unwrap();
        assert_eq!(
            wiki_article_url(&wp, "Alan Turing"),
            "https://en.wikipedia.org/wiki/Alan_Turing"
        );

        let arch = Url::parse("https://wiki.archlinux.org/api.php").unwrap();
        assert_eq!(
            wiki_article_url(&arch, "Systemd"),
            "https://wiki.archlinux.org/index.php?title=Systemd"
        );

        let custom = Url::parse("https://wiki.example.org/docs/api.php").unwrap();
        assert_eq!(
            wiki_article_url(&custom, "Getting Started"),
            "https://wiki.example.org/docs/index.php?title=Getting_Started"
        );

        let myapi = Url::parse("https://wiki.example.org/myapi.php").unwrap();
        assert_eq!(
            wiki_article_url(&myapi, "Page"),
            "https://wiki.example.org/index.php?title=Page"
        );
    }
}
