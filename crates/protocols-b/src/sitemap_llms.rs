//! Sitemap XML & `llms.txt` documentation index adapter (`sitemap_llms`, A1 §E #18).

use crate::util::{
    MAX_URL_BYTES, cap_title, check_allowed_dialect_keys, collapse_ws, decode_xml_bytes,
    dialect_int_range, dialect_str, hit_to_value, matches_term, norm_date, opt_limit, opt_str,
    params_map, parse_base_url, parse_untrusted_xml, resolve_http_url, send_request,
    tokenize_terms, unknown_op, word_set,
};
use flate2::read::GzDecoder;
use fsm_core::{
    FsmError, HttpRequest, NetClient, OpDecl, Param, ParamType, ProtocolAdapter, ProtocolOutput,
    ReturnKind, SrcCx,
};
use regex::Regex;
use roxmltree::Node;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::Read;
use std::sync::{Arc, LazyLock};
use url::Url;

const KIND: &str = "sitemap_llms";
const DEFAULT_MAX_BYTES: u64 = 4_194_304;
const MIN_MAX_BYTES: u64 = 65_536;
const MAX_MAX_BYTES: u64 = 16_777_216;

static MD_LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\(([^)\s]+)\)").expect("md link regex"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexMode {
    Auto,
    SitemapXml,
    LlmsTxt,
}

#[derive(Debug, Clone)]
struct IndexEntry {
    url: String,
    title: Option<String>,
    description: Option<String>,
    lastmod: Option<String>,
}

static OPS: LazyLock<Vec<OpDecl>> = LazyLock::new(|| {
    vec![
        OpDecl::new(
            "search",
            "Search URLs or Markdown links inside a sitemap.xml or llms.txt index.",
            ReturnKind::Hits,
        )
        .with_param(Param::new(
            "query",
            ParamType::String,
            "Optional keyword query to filter and rank index entries",
        ))
        .with_param(
            Param::new("limit", ParamType::Int, "Max hits returned")
                .with_default(json!(10))
                .with_range(1.0, 50.0),
        ),
        OpDecl::new(
            "list",
            "List raw entries from a sitemap.xml or llms.txt index.",
            ReturnKind::Records,
        )
        .with_param(
            Param::new("limit", ParamType::Int, "Max entries returned")
                .with_default(json!(50))
                .with_range(1.0, 500.0),
        ),
    ]
});

pub struct SitemapLlmsAdapter {
    net: Arc<dyn NetClient>,
}

impl SitemapLlmsAdapter {
    pub fn new(net: Arc<dyn NetClient>) -> Self {
        Self { net }
    }

    fn parse_mode(dialect: &BTreeMap<String, Value>) -> Result<IndexMode, FsmError> {
        match dialect_str(KIND, dialect, "mode")? {
            None | Some("auto") => Ok(IndexMode::Auto),
            Some("sitemap_xml") => Ok(IndexMode::SitemapXml),
            Some("llms_txt") => Ok(IndexMode::LlmsTxt),
            Some(other) => Err(FsmError::invalid(format!(
                "sitemap_llms dialect.mode must be auto|sitemap_xml|llms_txt (got {other:?})"
            ))),
        }
    }

    fn parse_max_bytes(dialect: &BTreeMap<String, Value>) -> Result<u64, FsmError> {
        Ok(dialect_int_range(
            KIND,
            dialect,
            "max_response_bytes",
            MIN_MAX_BYTES,
            MAX_MAX_BYTES,
        )?
        .unwrap_or(DEFAULT_MAX_BYTES))
    }

    fn url_looks_like_text(url: &Url) -> bool {
        let path = url.path().to_ascii_lowercase();
        path.ends_with(".txt") || path.ends_with(".md")
    }

    fn url_looks_like_sitemap_xml(url: &Url) -> bool {
        let path = url.path().to_ascii_lowercase();
        path.ends_with(".xml") || path.ends_with(".xml.gz")
    }

    fn accept_header(mode: IndexMode, parsed_url: &Url) -> &'static str {
        match mode {
            IndexMode::LlmsTxt => "text/plain, text/markdown",
            IndexMode::SitemapXml => "application/xml, text/xml",
            IndexMode::Auto => {
                if Self::url_looks_like_text(parsed_url) {
                    "text/plain, text/markdown"
                } else {
                    "application/xml, text/xml"
                }
            }
        }
    }

    fn effective_mode(mode: IndexMode, parsed_url: &Url, body: &str) -> IndexMode {
        match mode {
            IndexMode::SitemapXml => IndexMode::SitemapXml,
            IndexMode::LlmsTxt => IndexMode::LlmsTxt,
            IndexMode::Auto => {
                if Self::url_looks_like_text(parsed_url) {
                    IndexMode::LlmsTxt
                } else if Self::url_looks_like_sitemap_xml(parsed_url)
                    || body.trim_start().starts_with('<')
                {
                    IndexMode::SitemapXml
                } else {
                    IndexMode::LlmsTxt
                }
            }
        }
    }

    fn maybe_decompress_gzip(
        bytes: &[u8],
        max_bytes: u64,
        status: u16,
    ) -> Result<Cow<'_, [u8]>, FsmError> {
        if bytes.starts_with(&[0x1f, 0x8b]) {
            let mut decoder = GzDecoder::new(bytes).take(max_bytes.saturating_add(1));
            let mut out = Vec::new();
            decoder.read_to_end(&mut out).map_err(|e| {
                FsmError::upstream(
                    KIND,
                    Some(status),
                    format!("invalid gzip sitemap body: {e}"),
                )
            })?;
            if out.len() as u64 > max_bytes {
                return Err(FsmError::MaxBytesExceeded(max_bytes));
            }
            Ok(Cow::Owned(out))
        } else {
            Ok(Cow::Borrowed(bytes))
        }
    }

    fn title_from_url(url_str: &str) -> String {
        let Ok(u) = Url::parse(url_str) else {
            return url_str.to_owned();
        };
        let segs: Vec<&str> = u
            .path_segments()
            .map(|s| s.filter(|p| !p.is_empty()).collect())
            .unwrap_or_default();
        if segs.is_empty() {
            return u.host_str().unwrap_or(url_str).to_owned();
        }
        let chosen = if segs.last().is_some_and(|s| {
            let low = s.to_ascii_lowercase();
            low == "index.html" || low == "index.md" || low == "index.htm"
        }) && segs.len() >= 2
        {
            segs[segs.len() - 2]
        } else {
            segs[segs.len() - 1]
        };
        let stripped = chosen
            .strip_suffix(".html")
            .or_else(|| chosen.strip_suffix(".htm"))
            .or_else(|| chosen.strip_suffix(".md"))
            .or_else(|| chosen.strip_suffix(".xml"))
            .unwrap_or(chosen);
        let decoded = percent_encoding::percent_decode_str(stripped).decode_utf8_lossy();
        let words = decoded.replace(['_', '-'], " ");
        let clean = collapse_ws(&words);
        if clean.is_empty() {
            url_str.to_owned()
        } else {
            clean
        }
    }

    fn parse_sitemap_xml(xml: &str, status: u16) -> Result<Vec<IndexEntry>, FsmError> {
        let doc = parse_untrusted_xml(xml, KIND, status)?;
        let mut entries = Vec::new();
        for node in doc.descendants().filter(|n| {
            n.is_element()
                && (n.tag_name().name().eq_ignore_ascii_case("url")
                    || n.tag_name().name().eq_ignore_ascii_case("sitemap"))
        }) {
            let loc = node
                .children()
                .filter(Node::is_element)
                .find(|c| c.tag_name().name().eq_ignore_ascii_case("loc"))
                .and_then(|c| c.text())
                .map(str::trim)
                .filter(|s| {
                    s.len() <= MAX_URL_BYTES
                        && (s.starts_with("http://") || s.starts_with("https://"))
                });
            let Some(url) = loc else {
                continue;
            };
            let lastmod = node
                .children()
                .filter(Node::is_element)
                .find(|c| c.tag_name().name().eq_ignore_ascii_case("lastmod"))
                .and_then(|c| c.text())
                .and_then(|d| norm_date(Some(d)));
            entries.push(IndexEntry {
                title: Some(cap_title(&Self::title_from_url(url))),
                url: url.to_owned(),
                description: None,
                lastmod,
            });
        }
        Ok(entries)
    }

    fn parse_llms_txt(body: &str, base_url: &str) -> Vec<IndexEntry> {
        let mut entries = Vec::new();
        let mut in_fence = false;
        let mut current_section: Option<String> = None;

        for raw_line in body.lines() {
            let line = raw_line.trim();
            if line.starts_with("```") {
                in_fence = !in_fence;
                continue;
            }
            if in_fence || line.is_empty() {
                continue;
            }
            if let Some(heading) = line.strip_prefix('#') {
                let text = heading.trim_start_matches('#').trim();
                if !text.is_empty() {
                    current_section = Some(text.to_owned());
                }
                continue;
            }

            for caps in MD_LINK_RE.captures_iter(line) {
                let full_match = caps.get(0).expect("group 0");
                // Skip Markdown image links `![alt](src)` (M6).
                if full_match.start() > 0 && line.as_bytes()[full_match.start() - 1] == b'!' {
                    continue;
                }
                let raw_title = cap_title(caps.get(1).map_or("", |m| m.as_str()));
                let raw_href = caps.get(2).map_or("", |m| m.as_str()).trim();
                let Some(url) =
                    resolve_http_url(base_url, raw_href).filter(|u| u.len() <= MAX_URL_BYTES)
                else {
                    continue;
                };
                let after = line[full_match.end()..].trim();
                let desc = after
                    .strip_prefix(':')
                    .or_else(|| after.strip_prefix("—"))
                    .or_else(|| after.strip_prefix("–"))
                    .or_else(|| after.strip_prefix('-'))
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(collapse_ws)
                    .or_else(|| current_section.clone());

                entries.push(IndexEntry {
                    url,
                    title: (!raw_title.is_empty()).then_some(raw_title),
                    description: desc,
                    lastmod: None,
                });
            }
        }
        entries
    }

    fn search_entries(entries: Vec<IndexEntry>, query: Option<&str>, limit: usize) -> Vec<Value> {
        let Some(q) = query.map(str::trim).filter(|s| !s.is_empty()) else {
            return entries
                .into_iter()
                .take(limit)
                .map(Self::entry_to_hit)
                .collect();
        };

        let q_lower = q.to_lowercase();
        let terms = tokenize_terms(q);
        if terms.is_empty() {
            return entries
                .into_iter()
                .take(limit)
                .map(Self::entry_to_hit)
                .collect();
        }

        let mut scored: Vec<(bool, usize, usize, IndexEntry)> = entries
            .into_iter()
            .enumerate()
            .filter_map(|(idx, entry)| {
                let title_l = entry.title.as_deref().unwrap_or("").to_lowercase();
                let desc_l = entry.description.as_deref().unwrap_or("").to_lowercase();
                let url_l = entry.url.to_lowercase();
                let title_words = word_set(&title_l);
                let desc_words = word_set(&desc_l);
                let url_words = word_set(&url_l);

                let mut matched_terms = 0usize;
                let mut score = 0usize;
                for t in &terms {
                    let in_title = matches_term(&title_l, &title_words, t);
                    let in_url = matches_term(&url_l, &url_words, t);
                    let in_desc = matches_term(&desc_l, &desc_words, t);
                    if in_title || in_url || in_desc {
                        matched_terms += 1;
                        if in_title {
                            score += 12;
                        }
                        if in_url {
                            score += 8;
                        }
                        if in_desc {
                            score += 4;
                        }
                    }
                }
                if matched_terms == 0 {
                    return None;
                }
                let all_matched = matched_terms == terms.len();
                if title_l.contains(&q_lower) {
                    score += 30;
                } else if url_l.contains(&q_lower) {
                    score += 18;
                } else if desc_l.contains(&q_lower) {
                    score += 10;
                }
                Some((all_matched, score, idx, entry))
            })
            .collect();

        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| a.2.cmp(&b.2))
        });

        scored
            .into_iter()
            .take(limit)
            .map(|(_, _, _, e)| Self::entry_to_hit(e))
            .collect()
    }

    fn entry_to_hit(entry: IndexEntry) -> Value {
        let title = entry
            .title
            .unwrap_or_else(|| Self::title_from_url(&entry.url));
        let snippet = entry.description.unwrap_or_default();
        let date_source = entry.lastmod.as_ref().map(|_| "sitemap");
        hit_to_value(title, entry.url, snippet, entry.lastmod, date_source, None)
    }

    fn entry_to_record(entry: IndexEntry) -> Value {
        let mut map = serde_json::Map::new();
        map.insert("url".to_owned(), Value::String(entry.url));
        if let Some(title) = entry.title {
            map.insert("title".to_owned(), Value::String(title));
        }
        if let Some(description) = entry.description {
            map.insert("description".to_owned(), Value::String(description));
        }
        if let Some(lastmod) = entry.lastmod {
            map.insert("lastmod".to_owned(), Value::String(lastmod));
        }
        Value::Object(map)
    }
}

#[async_trait::async_trait]
impl ProtocolAdapter for SitemapLlmsAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn ops(&self) -> &[OpDecl] {
        &OPS
    }

    fn validate_dialect(&self, dialect: &BTreeMap<String, Value>) -> Result<(), FsmError> {
        check_allowed_dialect_keys(KIND, dialect, &["mode", "max_response_bytes"])?;
        Self::parse_mode(dialect)?;
        Self::parse_max_bytes(dialect)?;
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
        let (parsed_url, _) = parse_base_url(self.net.as_ref(), base_url)?;
        let mode = Self::parse_mode(&cx.dialect)?;
        let max_bytes = Self::parse_max_bytes(&cx.dialect)?;
        let map = params_map(&params)?;

        if op != "search" && op != "list" {
            return Err(unknown_op(KIND, op, &["search", "list"]));
        }

        let req = HttpRequest::get(base_url.trim())
            .header("Accept", Self::accept_header(mode, &parsed_url))
            .with_max_bytes(max_bytes);
        let resp = send_request(self.net.as_ref(), req, cx, KIND).await?;
        let raw_bytes = Self::maybe_decompress_gzip(&resp.body, max_bytes, resp.status)?;
        let body = decode_xml_bytes(&raw_bytes, resp.header("content-type"));

        let entries = match Self::effective_mode(mode, &parsed_url, &body) {
            IndexMode::SitemapXml => Self::parse_sitemap_xml(&body, resp.status)?,
            IndexMode::LlmsTxt => Self::parse_llms_txt(&body, base_url.trim()),
            IndexMode::Auto => unreachable!("effective_mode never returns Auto"),
        };

        let mut out = ProtocolOutput::new(KIND, base_url, op);
        match op {
            "search" => {
                let query = opt_str(map, "query")?;
                let limit = opt_limit(map, 10, 1, 50)?;
                out.items = Self::search_entries(entries, query.as_deref(), limit);
            }
            "list" => {
                let limit = opt_limit(map, 50, 1, 500)?;
                out.items = entries
                    .into_iter()
                    .take(limit)
                    .map(Self::entry_to_record)
                    .collect();
            }
            _ => unreachable!(),
        }
        Ok(out)
    }
}
