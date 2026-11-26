//! OpenSearch 1.1 OSDD + Atom/RSS/JSON protocol adapter (`opensearch`, A1 §E #3).

use crate::common::{
    attr_by_local, child_text, children_by_local, clip, decode_body, descendants_by_local,
    dialect_opt_bool, dialect_opt_str, dialect_opt_str_vec, hit_to_value, node_text,
    normalize_date, opt_int, opt_limit, params_map, parse_json, parse_xml,
    reject_unknown_dialect_keys, req_str, send_checked, strip_tags, validate_base_url,
};
use fsm_core::{
    FsmError, Hit, HttpRequest, OpDecl, Param, ParamType, ProtocolAdapter, ProtocolOutput,
    ReturnKind, SourceDeps, SrcCx,
};
use moka::future::Cache;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use url::Url;

const KIND: &str = "opensearch";
const DIALECT_KEYS: &[&str] = &[
    "prefer_mime",
    "url_template",
    "upgrade_https",
    "strip_unfilled_tokens",
];
const DEFAULT_PREFER_MIME: &[&str] = &[
    "application/atom+xml",
    "application/rss+xml",
    "application/json",
];
const OSDD_ACCEPT: &str = "application/opensearchdescription+xml, application/xml";
const MAX_OSDD_OFFSET: usize = 1_000_000;

static TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{([^{}]+)\}").expect("regex"));

#[derive(Debug, Clone)]
struct OsddUrl {
    media_type: String,
    template: String,
    rel: Option<String>,
    index_offset: usize,
    page_offset: usize,
}

#[derive(Debug, Clone)]
struct OsddDoc {
    short_name: Option<String>,
    long_name: Option<String>,
    description: Option<String>,
    contact: Option<String>,
    tags: Option<String>,
    urls: Vec<OsddUrl>,
}

pub struct OpenSearchAdapter {
    deps: SourceDeps,
    ops: Vec<OpDecl>,
    osdd_cache: Cache<String, Arc<OsddDoc>>,
}

impl OpenSearchAdapter {
    pub fn new(deps: SourceDeps) -> Self {
        let ops = vec![
            OpDecl::new(
                "search",
                "Search an OpenSearch 1.1 catalog (Atom, RSS, or JSON feed).",
                ReturnKind::Hits,
            )
            .with_param(Param::new("query", ParamType::String, "Search query").required())
            .with_param(
                Param::new("limit", ParamType::Int, "Maximum results to return")
                    .with_default(json!(10))
                    .with_range(1.0, 50.0),
            )
            .with_param(
                Param::new("page", ParamType::Int, "1-based result page")
                    .with_default(json!(1))
                    .with_range(1.0, 1000.0),
            ),
            OpDecl::new(
                "describe",
                "Fetch and parse an OpenSearch Description Document (OSDD).",
                ReturnKind::Records,
            ),
        ];
        let osdd_cache = Cache::builder()
            .max_capacity(256)
            .time_to_live(Duration::from_secs(3600))
            .build();
        Self {
            deps,
            ops,
            osdd_cache,
        }
    }

    async fn fetch_osdd(&self, base_url: &str, cx: &SrcCx) -> Result<Arc<OsddDoc>, FsmError> {
        if !cx.replay_mode
            && let Some(cached) = self.osdd_cache.get(base_url).await
        {
            return Ok(cached);
        }
        let req = HttpRequest::get(base_url).header("Accept", OSDD_ACCEPT);
        let resp = send_checked(&self.deps, req, cx, KIND).await?;
        let body = decode_body(&resp);
        let parsed = Arc::new(parse_osdd(&body, resp.status)?);
        if !cx.replay_mode {
            self.osdd_cache
                .insert(base_url.to_owned(), Arc::clone(&parsed))
                .await;
        }
        Ok(parsed)
    }

    async fn call_describe(&self, base_url: &str, cx: &SrcCx) -> Result<ProtocolOutput, FsmError> {
        let osdd = self.fetch_osdd(base_url, cx).await?;
        let mut rec = Map::new();
        rec.insert("url".into(), Value::String(base_url.to_owned()));
        if let Some(v) = &osdd.short_name {
            rec.insert("shortName".into(), Value::String(v.clone()));
            rec.insert("title".into(), Value::String(v.clone()));
        }
        if let Some(v) = &osdd.long_name {
            rec.insert("longName".into(), Value::String(v.clone()));
        }
        if let Some(v) = &osdd.description {
            rec.insert("description".into(), Value::String(v.clone()));
        }
        if let Some(v) = &osdd.contact {
            rec.insert("contact".into(), Value::String(v.clone()));
        }
        if let Some(v) = &osdd.tags {
            rec.insert("tags".into(), Value::String(v.clone()));
        }
        let templates: Vec<Value> = osdd
            .urls
            .iter()
            .map(|u| {
                let mut m = Map::new();
                m.insert("type".into(), Value::String(u.media_type.clone()));
                m.insert("template".into(), Value::String(u.template.clone()));
                if let Some(r) = &u.rel {
                    m.insert("rel".into(), Value::String(r.clone()));
                }
                m.insert("indexOffset".into(), json!(u.index_offset));
                m.insert("pageOffset".into(), json!(u.page_offset));
                Value::Object(m)
            })
            .collect();
        rec.insert("templates".into(), Value::Array(templates));

        let mut out = ProtocolOutput::new(KIND, base_url, "describe");
        out.items = vec![Value::Object(rec)];
        Ok(out)
    }

    async fn call_search(
        &self,
        base_url: &str,
        params: &Map<String, Value>,
        cx: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError> {
        let query = req_str(params, "query")?;
        let limit = opt_limit(params, "limit", 10, 1, 50)?;
        let explicit_page = opt_int(params, "page", 1, 1000)?;
        let page = explicit_page.unwrap_or(1) as usize;

        let prefer_mime =
            dialect_opt_str_vec(KIND, &cx.dialect, "prefer_mime")?.unwrap_or_else(|| {
                DEFAULT_PREFER_MIME
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect()
            });
        let url_template_override = dialect_opt_str(KIND, &cx.dialect, "url_template")?;
        let upgrade_https = dialect_opt_bool(KIND, &cx.dialect, "upgrade_https")?.unwrap_or(true);
        let strip_unfilled =
            dialect_opt_bool(KIND, &cx.dialect, "strip_unfilled_tokens")?.unwrap_or(true);

        let selected = if let Some(tpl) = url_template_override {
            let mime = infer_mime_from_template(&tpl, &prefer_mime);
            OsddUrl {
                media_type: mime,
                template: tpl,
                rel: None,
                index_offset: 1,
                page_offset: 1,
            }
        } else {
            let osdd = self.fetch_osdd(base_url, cx).await?;
            select_osdd_url(&osdd, &prefer_mime).ok_or_else(|| {
                FsmError::upstream(
                    KIND,
                    None,
                    format!(
                        "OSDD at {base_url} offers no <Url> matching prefer_mime {prefer_mime:?}"
                    ),
                )
            })?
        };

        let expanded = expand_template(
            &selected,
            &query,
            limit,
            page,
            explicit_page.is_some(),
            cx.lang.as_deref(),
            upgrade_https,
            strip_unfilled,
        )?;

        let accept = accept_header_for_mime(&selected.media_type);
        let req = HttpRequest::get(expanded).header("Accept", accept);
        let resp = send_checked(&self.deps, req, cx, KIND).await?;

        let is_json = selected.media_type.contains("json")
            || resp.media_type().is_some_and(|m| m.contains("json"));

        let (mut hits, next_cursor) = if is_json {
            let val = parse_json(&resp, KIND)?;
            parse_json_feed(&val, resp.status, page, limit)?
        } else {
            let body = decode_body(&resp);
            let trimmed = body.trim_start();
            if trimmed.starts_with('{') || trimmed.starts_with('[') {
                let val = parse_json(&resp, KIND)?;
                parse_json_feed(&val, resp.status, page, limit)?
            } else {
                parse_xml_feed(&body, resp.status, page, limit)?
            }
        };

        hits.truncate(limit);
        let mut out = ProtocolOutput::new(KIND, base_url, "search");
        out.items = hits.into_iter().map(hit_to_value).collect();
        out.next_cursor = next_cursor;
        Ok(out)
    }
}

fn parse_osdd(body: &str, status: u16) -> Result<OsddDoc, FsmError> {
    let doc = parse_xml(body, KIND, status)?;
    let root = doc.root_element();
    if !root
        .tag_name()
        .name()
        .eq_ignore_ascii_case("OpenSearchDescription")
    {
        return Err(FsmError::upstream(
            KIND,
            Some(status),
            format!(
                "expected <OpenSearchDescription>, got <{}>",
                root.tag_name().name()
            ),
        ));
    }

    let mut urls = Vec::new();
    for u in children_by_local(root, "Url") {
        let Some(template) = u
            .attribute("template")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let media_type = u
            .attribute("type")
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let rel = u.attribute("rel").map(|s| s.trim().to_ascii_lowercase());
        let index_offset = u
            .attribute("indexOffset")
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(1)
            .min(MAX_OSDD_OFFSET);
        let page_offset = u
            .attribute("pageOffset")
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(1)
            .min(MAX_OSDD_OFFSET);
        urls.push(OsddUrl {
            media_type,
            template: template.to_owned(),
            rel,
            index_offset,
            page_offset,
        });
    }

    Ok(OsddDoc {
        short_name: child_text(root, "ShortName"),
        long_name: child_text(root, "LongName"),
        description: child_text(root, "Description"),
        contact: child_text(root, "Contact"),
        tags: child_text(root, "Tags"),
        urls,
    })
}

fn select_osdd_url(osdd: &OsddDoc, prefer_mime: &[String]) -> Option<OsddUrl> {
    let candidates: Vec<&OsddUrl> = osdd
        .urls
        .iter()
        .filter(|u| {
            !u.media_type.contains("suggestions")
                && u.rel.as_deref() != Some("suggestions")
                && u.rel.as_deref() != Some("self")
        })
        .collect();

    for want in prefer_mime {
        let want_lower = want.trim().to_ascii_lowercase();
        if let Some(found) = candidates.iter().find(|u| u.media_type == want_lower) {
            return Some((*found).clone());
        }
    }
    candidates.first().map(|u| (*u).clone())
}

fn infer_mime_from_template(tpl: &str, prefer_mime: &[String]) -> String {
    let lower = tpl.to_ascii_lowercase();
    if lower.contains("format=json") || lower.contains(".json") {
        "application/json".to_owned()
    } else if lower.contains("format=rss") || lower.contains(".rss") || lower.contains("rss.") {
        "application/rss+xml".to_owned()
    } else if lower.contains("format=atom") || lower.contains(".atom") {
        "application/atom+xml".to_owned()
    } else {
        prefer_mime
            .first()
            .cloned()
            .unwrap_or_else(|| "application/atom+xml".to_owned())
    }
}

fn accept_header_for_mime(mime: &str) -> &'static str {
    if mime.contains("atom") {
        "application/atom+xml, application/xml"
    } else if mime.contains("rss") {
        "application/rss+xml, application/xml"
    } else if mime.contains("json") {
        "application/json"
    } else {
        "application/atom+xml, application/rss+xml, application/xml"
    }
}

#[allow(clippy::too_many_arguments)]
fn expand_template(
    osdd_url: &OsddUrl,
    query: &str,
    limit: usize,
    page: usize,
    explicit_page: bool,
    lang: Option<&str>,
    upgrade_https: bool,
    strip_unfilled: bool,
) -> Result<String, FsmError> {
    let mut tpl = osdd_url.template.trim().to_owned();
    if upgrade_https && let Some(rest) = tpl.strip_prefix("http://") {
        tpl = format!("https://{rest}");
    }

    let (base_part, query_part) = match tpl.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (tpl.as_str(), None),
    };

    // Inspect which standard OpenSearch tokens exist anywhere in the template.
    let mut has_count = false;
    let mut has_start_page = false;
    let mut has_start_index = false;
    for caps in TOKEN_RE.captures_iter(&tpl) {
        let local = token_local_name(&caps[1]);
        match local {
            "count" => has_count = true,
            "startPage" => has_start_page = true,
            "startIndex" => has_start_index = true,
            _ => {}
        }
    }

    // Fill startPage when caller requested page, OR when startPage is the sole
    // pagination token (matches ERDDAP `page={startPage?}` while keeping NASA CMR
    // clean when both `cursor={os:startPage?}` and `offset={os:startIndex?}` exist).
    let fill_start_page = explicit_page || (has_start_page && !has_start_index);
    let fill_start_index = explicit_page && page > 1 && !has_start_page;
    let start_index_val = page
        .saturating_sub(1)
        .saturating_mul(limit)
        .saturating_add(osdd_url.index_offset);
    let start_page_val = page.saturating_sub(1).saturating_add(osdd_url.page_offset);

    // Capture the original template's scheme & host before expanding path tokens so
    // caller-controlled values like `{language}` cannot hijack host/scheme/fragment (I5).
    let orig_base_url = Url::parse(base_part).ok();

    // Expand any tokens in the path component.
    let expanded_base = expand_segment(
        base_part,
        query,
        limit,
        start_page_val,
        start_index_val,
        fill_start_page,
        fill_start_index,
        lang,
        strip_unfilled,
        true,
    )?
    .unwrap_or_else(|| base_part.to_owned());

    let mut url = Url::parse(&expanded_base).map_err(|e| {
        FsmError::upstream(
            KIND,
            None,
            format!("invalid OpenSearch template URL {expanded_base:?}: {e}"),
        )
    })?;
    if let Some(orig) = orig_base_url
        && (url.scheme() != orig.scheme()
            || url.host_str() != orig.host_str()
            || url.fragment() != orig.fragment())
    {
        return Err(FsmError::invalid(format!(
            "{KIND}: template expansion altered URL scheme, host, or fragment"
        )));
    }

    let mut pairs: Vec<(String, String)> = Vec::new();
    if let Some(qs) = query_part {
        // Decode template literals once (preserving `{tokens}` verbatim) so
        // pre-encoded values like `fq=type%3Aarticle&sort=date+desc` are not double-encoded.
        for (raw_k, raw_v) in url::form_urlencoded::parse(qs.as_bytes()) {
            if raw_k.is_empty() && raw_v.is_empty() {
                continue;
            }
            // Drop malformed unclosed optional tokens like NOAA NCEI's `{time:start?/{time:end?}`
            // or empty template params like NASA CMR's `clientId=` when stripping unfilled tokens.
            if strip_unfilled && (raw_v.is_empty() || has_malformed_optional_token(&raw_v)) {
                continue;
            }
            let Some(expanded_v) = expand_segment(
                &raw_v,
                query,
                limit,
                start_page_val,
                start_index_val,
                fill_start_page,
                fill_start_index,
                lang,
                strip_unfilled,
                false,
            )?
            else {
                continue;
            };
            pairs.push((raw_k.into_owned(), expanded_v));
        }
    }

    // If the OSDD template had no `{count}` token (e.g. CiNii Research/KAKEN),
    // append standard `count={limit}`.
    if !has_count && !pairs.iter().any(|(k, _)| k == "count") {
        pairs.push(("count".to_owned(), limit.to_string()));
    }

    if !pairs.is_empty() {
        let mut qp = url.query_pairs_mut();
        for (k, v) in pairs {
            qp.append_pair(&k, &v);
        }
    }

    Ok(url.to_string())
}

fn token_local_name(raw_inner: &str) -> &str {
    let no_opt = raw_inner.trim().trim_end_matches('?');
    no_opt.rsplit_once(':').map_or(no_opt, |(_, local)| local)
}

fn has_malformed_optional_token(v: &str) -> bool {
    // Detect unclosed `{...` (e.g. `{time:start?/{time:end?}`) by checking brace balance.
    let mut depth: i32 = 0;
    for ch in v.chars() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth < 0 {
                    return true;
                }
            }
            _ => {}
        }
    }
    depth != 0
}

fn is_safe_path_lang_tag(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    (2..=16).contains(&bytes.len())
        && bytes[0].is_ascii_alphabetic()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_')
}

#[allow(clippy::too_many_arguments)]
fn expand_segment(
    segment: &str,
    query: &str,
    limit: usize,
    start_page_val: usize,
    start_index_val: usize,
    fill_start_page: bool,
    fill_start_index: bool,
    lang: Option<&str>,
    strip_unfilled: bool,
    is_path: bool,
) -> Result<Option<String>, FsmError> {
    if !segment.contains('{') {
        return Ok(Some(segment.to_owned()));
    }
    let mut out = String::with_capacity(segment.len());
    let mut last = 0;
    let mut dropped_optional = false;

    for caps in TOKEN_RE.captures_iter(segment) {
        let m = caps.get(0).expect("group 0");
        out.push_str(&segment[last..m.start()]);
        last = m.end();

        let raw = caps[1].trim();
        let optional = raw.ends_with('?');
        let local = token_local_name(raw);

        let replacement: Option<String> = match local {
            "searchTerms" => {
                if is_path {
                    Some(
                        percent_encoding::utf8_percent_encode(
                            query,
                            crate::common::PATH_SEGMENT_SET,
                        )
                        .to_string(),
                    )
                } else {
                    Some(query.to_owned())
                }
            }
            "count" => Some(limit.to_string()),
            "startPage" if fill_start_page || !optional => Some(start_page_val.to_string()),
            "startIndex" if fill_start_index || !optional => Some(start_index_val.to_string()),
            "inputEncoding" | "outputEncoding" if !optional => Some("UTF-8".to_owned()),
            "language" => match lang {
                Some(l) if is_path => {
                    let trimmed = l.trim();
                    if is_safe_path_lang_tag(trimmed) {
                        Some(trimmed.to_owned())
                    } else {
                        return Err(FsmError::invalid(format!(
                            "{KIND}: unsafe language tag {l:?} in URL template path"
                        )));
                    }
                }
                Some(l) => Some(l.to_owned()),
                None => None,
            },
            _ => None,
        };

        match replacement {
            Some(val) => out.push_str(&val),
            None if strip_unfilled => {
                if !is_path {
                    dropped_optional = true;
                }
            }
            None => out.push_str(m.as_str()),
        }
    }
    out.push_str(&segment[last..]);

    if dropped_optional && !is_path {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}

fn parse_xml_feed(
    body: &str,
    status: u16,
    page: usize,
    limit: usize,
) -> Result<(Vec<Hit>, Option<String>), FsmError> {
    let doc = parse_xml(body, KIND, status)?;
    let root = doc.root_element();
    let root_name = root.tag_name().name().to_ascii_lowercase();
    if !matches!(root_name.as_str(), "feed" | "rss" | "rdf") {
        return Err(FsmError::upstream(
            KIND,
            Some(status),
            format!(
                "unexpected OpenSearch XML feed root <{}> (expected feed, rss, or RDF)",
                root.tag_name().name()
            ),
        ));
    }

    let total_results = descendants_by_local(root, "totalResults")
        .next()
        .and_then(|n| node_text(n).parse::<usize>().ok());
    let start_index = descendants_by_local(root, "startIndex")
        .next()
        .and_then(|n| node_text(n).parse::<usize>().ok())
        .unwrap_or(1);

    let hits = if root_name == "feed" {
        parse_atom_entries(root)
    } else {
        parse_rss_items(root)
    };

    let next_cursor = total_results.and_then(|total| {
        let shown = start_index
            .saturating_sub(1)
            .saturating_add(hits.len().min(limit));
        (shown < total && !hits.is_empty())
            .then(|| page.checked_add(1).map(|p| p.to_string()))
            .flatten()
    });

    Ok((hits, next_cursor))
}

fn parse_atom_entries(root: roxmltree::Node<'_, '_>) -> Vec<Hit> {
    let mut hits = Vec::new();
    for entry in children_by_local(root, "entry") {
        let title = child_text(entry, "title")
            .map(|t| strip_tags(&t))
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let url = pick_atom_link(entry).unwrap_or_default();
        if url.is_empty() {
            continue;
        }
        let snippet = child_text(entry, "summary")
            .or_else(|| child_text(entry, "content"))
            .or_else(|| child_text(entry, "description"))
            .map(|s| clip(&strip_tags(&s), 400))
            .unwrap_or_default();
        let date = child_text(entry, "published")
            .or_else(|| child_text(entry, "publicationDate"))
            .or_else(|| child_text(entry, "updated"))
            .or_else(|| child_text(entry, "date"))
            .and_then(|d| normalize_date(&d));

        let mut hit = Hit::new(title, url);
        hit.snippet = snippet;
        if let Some(d) = date {
            hit.date = Some(d);
            hit.date_source = Some("api".to_owned());
        }
        hits.push(hit);
    }
    hits
}

fn pick_atom_link(entry: roxmltree::Node<'_, '_>) -> Option<String> {
    let links: Vec<(Option<&str>, Option<&str>, &str)> = children_by_local(entry, "link")
        .filter_map(|l| {
            let href = attr_by_local(l, "href")?.trim();
            if !(href.starts_with("http://") || href.starts_with("https://")) {
                return None;
            }
            Some((attr_by_local(l, "rel"), attr_by_local(l, "type"), href))
        })
        .collect();

    // 1. rel=alternate (or omitted) with text/html
    if let Some((_, _, href)) = links.iter().find(|(rel, ty, _)| {
        matches!(*rel, None | Some("alternate"))
            && ty.is_some_and(|t| t.to_ascii_lowercase().contains("text/html"))
    }) {
        return Some((*href).to_owned());
    }
    // 2. rel=alternate or no rel without an explicit non-html machine type
    if let Some((_, _, href)) = links
        .iter()
        .find(|(rel, ty, _)| matches!(*rel, None | Some("alternate")) && ty.is_none())
    {
        return Some((*href).to_owned());
    }
    // 3. rel=via (used by NASA CMR for landing pages)
    if let Some((_, _, href)) = links.iter().find(|(rel, _, _)| *rel == Some("via")) {
        return Some((*href).to_owned());
    }
    // 4. any rel=alternate
    if let Some((_, _, href)) = links.iter().find(|(rel, _, _)| *rel == Some("alternate")) {
        return Some((*href).to_owned());
    }
    // 5. any http(s) link
    if let Some((_, _, href)) = links.first() {
        return Some((*href).to_owned());
    }
    // 6. <id> if http(s)
    child_text(entry, "id").filter(|id| id.starts_with("http://") || id.starts_with("https://"))
}

fn parse_rss_items(root: roxmltree::Node<'_, '_>) -> Vec<Hit> {
    let mut hits = Vec::new();
    for item in descendants_by_local(root, "item") {
        let title = child_text(item, "title")
            .map(|t| strip_tags(&t))
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let url = child_text(item, "link")
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
            .or_else(|| {
                child_text(item, "guid")
                    .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
            })
            .unwrap_or_default();
        if url.is_empty() {
            continue;
        }
        let snippet = child_text(item, "description")
            .map(|s| clip(&strip_tags(&s), 400))
            .unwrap_or_default();
        let date = child_text(item, "pubDate")
            .or_else(|| child_text(item, "date"))
            .and_then(|d| normalize_date(&d));

        let mut hit = Hit::new(title, url);
        hit.snippet = snippet;
        if let Some(d) = date {
            hit.date = Some(d);
            hit.date_source = Some("api".to_owned());
        }
        hits.push(hit);
    }
    hits
}

fn parse_json_feed(
    val: &Value,
    status: u16,
    page: usize,
    limit: usize,
) -> Result<(Vec<Hit>, Option<String>), FsmError> {
    if let Some(err) = val.get("error")
        && !err.is_null()
        && *err != Value::Bool(false)
    {
        let msg = val
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| err.as_str())
            .unwrap_or("OpenSearch JSON error");
        return Err(FsmError::upstream(KIND, Some(status), msg));
    }

    let items = val
        .get("items")
        .or_else(|| val.get("entries"))
        .or_else(|| val.get("results"))
        .and_then(Value::as_array)
        .or_else(|| val.as_array())
        .ok_or_else(|| {
            FsmError::upstream(
                KIND,
                Some(status),
                "OpenSearch JSON response missing items/entries/results array",
            )
        })?;

    let mut hits = Vec::new();
    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let title = obj
            .get("title")
            .and_then(Value::as_str)
            .map(strip_tags)
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let url = obj
            .get("url")
            .and_then(Value::as_str)
            .or_else(|| obj.get("@id").and_then(Value::as_str))
            .or_else(|| {
                obj.get("link")
                    .and_then(|l| l.as_str().or_else(|| l.get("@id").and_then(Value::as_str)))
            })
            .or_else(|| obj.get("id").and_then(Value::as_str))
            .unwrap_or("")
            .to_owned();
        if !url.starts_with("http://") && !url.starts_with("https://") {
            continue;
        }
        let snippet = obj
            .get("summary")
            .or_else(|| obj.get("description"))
            .or_else(|| obj.get("dc:description"))
            .and_then(Value::as_str)
            .map(|s| clip(&strip_tags(s), 400))
            .unwrap_or_default();
        let date = obj
            .get("prism:publicationDate")
            .or_else(|| obj.get("published"))
            .or_else(|| obj.get("updated"))
            .or_else(|| obj.get("dc:date"))
            .and_then(Value::as_str)
            .and_then(normalize_date);

        let mut hit = Hit::new(title, url);
        hit.snippet = snippet;
        if let Some(d) = date {
            hit.date = Some(d);
            hit.date_source = Some("api".to_owned());
        }
        hits.push(hit);
    }

    let total = val
        .get("opensearch:totalResults")
        .or_else(|| val.get("totalResults"))
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        });
    let next_cursor = total.and_then(|t| {
        let shown = (page.saturating_sub(1) as u64)
            .saturating_mul(limit as u64)
            .saturating_add(hits.len().min(limit) as u64);
        (shown < t && !hits.is_empty())
            .then(|| page.checked_add(1).map(|p| p.to_string()))
            .flatten()
    });

    Ok((hits, next_cursor))
}

#[async_trait::async_trait]
impl ProtocolAdapter for OpenSearchAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn ops(&self) -> &[OpDecl] {
        &self.ops
    }

    fn validate_dialect(&self, dialect: &BTreeMap<String, Value>) -> Result<(), FsmError> {
        reject_unknown_dialect_keys(KIND, dialect, DIALECT_KEYS)?;
        let _ = dialect_opt_str_vec(KIND, dialect, "prefer_mime")?;
        if let Some(tpl) = dialect_opt_str(KIND, dialect, "url_template")?
            && !(tpl.starts_with("http://") || tpl.starts_with("https://"))
        {
            return Err(FsmError::invalid(format!(
                "{KIND}: dialect.url_template must start with http:// or https://"
            )));
        }
        let _ = dialect_opt_bool(KIND, dialect, "upgrade_https")?;
        let _ = dialect_opt_bool(KIND, dialect, "strip_unfilled_tokens")?;
        Ok(())
    }

    async fn call(
        &self,
        base_url: &str,
        op: &str,
        params: Value,
        cx: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError> {
        let _ = validate_base_url(base_url)?;
        self.validate_dialect(&cx.dialect)?;
        let map = params_map(&params)?;
        match op {
            "search" => self.call_search(base_url, map, cx).await,
            "describe" => self.call_describe(base_url, cx).await,
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
    fn template_hygiene_strips_malformed_tokens_and_upgrades_https() {
        let u = OsddUrl {
            media_type: "application/atom+xml".into(),
            template: "http://www.ncei.noaa.gov/opensearch?q={searchTerms}&start={startIndex?}&num={count?}&time={time:start?/{time:end?}&empty=&f=atom".into(),
            rel: None,
            index_offset: 1,
            page_offset: 1,
        };
        let url = expand_template(&u, "temp", 3, 1, false, None, true, true).unwrap();
        assert_eq!(
            url,
            "https://www.ncei.noaa.gov/opensearch?q=temp&num=3&f=atom"
        );

        let url_p2 = expand_template(&u, "temp", 3, 2, true, None, false, true).unwrap();
        assert_eq!(
            url_p2,
            "http://www.ncei.noaa.gov/opensearch?q=temp&start=4&num=3&f=atom"
        );
    }

    #[test]
    fn osdd_selector_ignores_suggestions_and_honors_prefer_mime() {
        let xml = r#"<OpenSearchDescription xmlns="http://a9.com/-/spec/opensearch/1.1/">
            <Url type="application/x-suggestions+json" template="https://ex.org/sug?q={searchTerms}"/>
            <Url type="application/rss+xml" template="https://ex.org/rss?q={searchTerms}"/>
            <Url type="application/atom+xml" template="https://ex.org/atom?q={searchTerms}"/>
        </OpenSearchDescription>"#;
        let doc = parse_osdd(xml, 200).unwrap();
        let default_pref: Vec<String> = DEFAULT_PREFER_MIME
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(
            select_osdd_url(&doc, &default_pref).unwrap().media_type,
            "application/atom+xml"
        );
        assert_eq!(
            select_osdd_url(&doc, &["application/rss+xml".into()])
                .unwrap()
                .media_type,
            "application/rss+xml"
        );
    }
}
