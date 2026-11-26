//! RSS 2.0 / Atom 1.0 syndication feed adapter (`rss_atom`, A1 §E #15).

use crate::util::{
    MAX_URL_BYTES, cap_title, check_allowed_dialect_keys, collapse_ws, decode_xml_body,
    dialect_int_range, dialect_str, hit_to_value, matches_term, norm_date, opt_limit, opt_str,
    params_map, parse_base_url, parse_untrusted_xml, resolve_http_url, send_request, strip_tags,
    tokenize_terms, unescape_html, unknown_op, word_set,
};
use fsm_core::text::smart_truncate;
use fsm_core::{
    FsmError, HttpRequest, NetClient, OpDecl, Param, ParamType, ProtocolAdapter, ProtocolOutput,
    ReturnKind, SrcCx,
};
use roxmltree::{Document, Node};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

const KIND: &str = "rss_atom";
const DEFAULT_MAX_BYTES: u64 = 2_097_152;
const MIN_MAX_BYTES: u64 = 65_536;
const MAX_MAX_BYTES: u64 = 10_485_760;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeedFormat {
    Auto,
    Rss,
    Atom,
}

static OPS: LazyLock<Vec<OpDecl>> = LazyLock::new(|| {
    vec![
        OpDecl::new(
            "search",
            "Fetch an RSS 2.0 or Atom 1.0 feed and optionally filter entries by keyword.",
            ReturnKind::Hits,
        )
        .with_param(Param::new(
            "query",
            ParamType::String,
            "Optional keyword filter (omitted returns latest items)",
        ))
        .with_param(
            Param::new("limit", ParamType::Int, "Max items returned")
                .with_default(json!(10))
                .with_range(1.0, 50.0),
        ),
        OpDecl::new(
            "latest",
            "Fetch latest entries from an RSS 2.0 or Atom 1.0 feed.",
            ReturnKind::Hits,
        )
        .with_param(
            Param::new("limit", ParamType::Int, "Max items returned")
                .with_default(json!(10))
                .with_range(1.0, 50.0),
        ),
    ]
});

struct FeedEntry {
    title: String,
    url: String,
    snippet: String,
    match_summary: String,
    date: Option<String>,
}

pub struct RssAtomAdapter {
    net: Arc<dyn NetClient>,
}

impl RssAtomAdapter {
    pub fn new(net: Arc<dyn NetClient>) -> Self {
        Self { net }
    }

    fn parse_format(dialect: &BTreeMap<String, Value>) -> Result<FeedFormat, FsmError> {
        match dialect_str(KIND, dialect, "format")? {
            None | Some("auto") => Ok(FeedFormat::Auto),
            Some("rss") => Ok(FeedFormat::Rss),
            Some("atom") => Ok(FeedFormat::Atom),
            Some(other) => Err(FsmError::invalid(format!(
                "rss_atom dialect.format must be auto|rss|atom (got {other:?})"
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

    fn accept_header(format: FeedFormat, base_url: &str) -> &'static str {
        match format {
            FeedFormat::Atom => "application/atom+xml, application/xml, text/xml",
            FeedFormat::Rss => "application/rss+xml, application/xml, text/xml",
            FeedFormat::Auto => {
                if base_url.to_ascii_lowercase().contains("atom") {
                    "application/atom+xml, application/xml, text/xml"
                } else {
                    "application/rss+xml, application/xml, text/xml"
                }
            }
        }
    }

    fn child_text<'a>(node: Node<'a, 'a>, local_name: &str) -> Option<String> {
        for child in node.children().filter(Node::is_element) {
            if child.tag_name().name().eq_ignore_ascii_case(local_name) {
                let mut buf = String::new();
                for sub in child.descendants().filter(Node::is_text) {
                    if let Some(t) = sub.text() {
                        buf.push_str(t);
                    }
                }
                let trimmed = buf.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_owned());
                }
            }
        }
        None
    }

    fn atom_title(entry: Node<'_, '_>) -> String {
        let Some(node) = entry
            .children()
            .find(|c| c.is_element() && c.tag_name().name().eq_ignore_ascii_case("title"))
        else {
            return String::new();
        };
        let mut buf = String::new();
        for sub in node.descendants().filter(Node::is_text) {
            if let Some(t) = sub.text() {
                buf.push_str(t);
            }
        }
        let raw = buf.trim();
        if raw.is_empty() {
            return String::new();
        }
        // RFC 4287 §3.1.1: default/text is plain text; xhtml text descendants are already plain;
        // decode double-escaped HTML entities without stripping `<...>` (I5, N3).
        // Only type="html" carries escaped HTML markup to strip.
        let cleaned = match node
            .attribute("type")
            .map(|s| s.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("html") => strip_tags(raw),
            _ => collapse_ws(&unescape_html(raw)),
        };
        cap_title(&cleaned)
    }

    fn parse_rss_items(doc: &Document<'_>, effective_base: &str) -> Vec<FeedEntry> {
        let mut entries = Vec::new();
        for item in doc
            .descendants()
            .filter(|n| n.is_element() && n.tag_name().name().eq_ignore_ascii_case("item"))
        {
            let title = Self::child_text(item, "title")
                .map(|t| cap_title(&unescape_html(&t)))
                .unwrap_or_default();

            let raw_link = Self::child_text(item, "link")
                .or_else(|| {
                    item.children()
                        .filter(|c| {
                            c.is_element() && c.tag_name().name().eq_ignore_ascii_case("link")
                        })
                        .find_map(|c| c.attribute("href").map(str::to_owned))
                })
                .or_else(|| {
                    item.children()
                        .filter(|c| {
                            c.is_element() && c.tag_name().name().eq_ignore_ascii_case("guid")
                        })
                        .filter(|c| c.attribute("isPermaLink") != Some("false"))
                        .find_map(|c| {
                            c.text()
                                .map(str::trim)
                                .filter(|t| t.starts_with("http"))
                                .map(str::to_owned)
                        })
                });

            let url = raw_link
                .as_deref()
                .and_then(|href| resolve_http_url(effective_base, href))
                .filter(|u| u.len() <= MAX_URL_BYTES)
                .unwrap_or_default();
            if title.is_empty() && url.is_empty() {
                continue;
            }

            let raw_desc = Self::child_text(item, "description")
                .or_else(|| Self::child_text(item, "encoded"))
                .or_else(|| Self::child_text(item, "summary"))
                .unwrap_or_default();
            let match_summary = strip_tags(&raw_desc);
            let snippet = if match_summary.is_empty() {
                String::new()
            } else {
                smart_truncate(&match_summary, 400).0
            };

            let date = Self::child_text(item, "pubDate")
                .or_else(|| Self::child_text(item, "date"))
                .or_else(|| Self::child_text(item, "published"))
                .or_else(|| Self::child_text(item, "updated"))
                .and_then(|d| norm_date(Some(&d)));

            entries.push(FeedEntry {
                title,
                url,
                snippet,
                match_summary,
                date,
            });
        }
        entries
    }

    fn parse_atom_entries(doc: &Document<'_>, effective_base: &str) -> Vec<FeedEntry> {
        let mut entries = Vec::new();
        for entry in doc
            .descendants()
            .filter(|n| n.is_element() && n.tag_name().name().eq_ignore_ascii_case("entry"))
        {
            let title = Self::atom_title(entry);

            let mut best_link: Option<&str> = None;
            let mut fallback_link: Option<&str> = None;
            for link in entry
                .children()
                .filter(|c| c.is_element() && c.tag_name().name().eq_ignore_ascii_case("link"))
            {
                let Some(href) = link
                    .attribute("href")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                else {
                    continue;
                };
                match link.attribute("rel").map(str::trim) {
                    None | Some("") | Some("alternate") => {
                        if best_link.is_none() {
                            best_link = Some(href);
                        }
                    }
                    Some("self") | Some("replies") | Some("edit") | Some("enclosure") => {}
                    _ => {
                        if fallback_link.is_none() {
                            fallback_link = Some(href);
                        }
                    }
                }
            }
            let raw_link = best_link.or(fallback_link).map(str::to_owned).or_else(|| {
                Self::child_text(entry, "id")
                    .filter(|id| id.starts_with("http://") || id.starts_with("https://"))
            });

            let url = raw_link
                .as_deref()
                .and_then(|href| resolve_http_url(effective_base, href))
                .filter(|u| u.len() <= MAX_URL_BYTES)
                .unwrap_or_default();
            if title.is_empty() && url.is_empty() {
                continue;
            }

            let raw_summary = Self::child_text(entry, "summary")
                .or_else(|| Self::child_text(entry, "content"))
                .unwrap_or_default();
            let match_summary = strip_tags(&raw_summary);
            let snippet = if match_summary.is_empty() {
                String::new()
            } else {
                smart_truncate(&match_summary, 400).0
            };

            let date = Self::child_text(entry, "published")
                .or_else(|| Self::child_text(entry, "updated"))
                .and_then(|d| norm_date(Some(&d)));

            entries.push(FeedEntry {
                title,
                url,
                snippet,
                match_summary,
                date,
            });
        }
        entries
    }

    fn parse_feed(
        xml: &str,
        base_url: &str,
        format: FeedFormat,
        status: u16,
    ) -> Result<Vec<FeedEntry>, FsmError> {
        let doc = parse_untrusted_xml(xml, KIND, status)?;
        let root = doc.root_element();
        let root_name = root.tag_name().name();
        let effective_base = root
            .attribute(("http://www.w3.org/XML/1998/namespace", "base"))
            .or_else(|| root.attribute("base"))
            .filter(|b| b.starts_with("http://") || b.starts_with("https://"))
            .unwrap_or(base_url);

        if root_name.eq_ignore_ascii_case("feed") {
            if format == FeedFormat::Rss {
                return Err(FsmError::upstream(
                    KIND,
                    Some(status),
                    "expected RSS feed (dialect.format=rss) but received Atom <feed>",
                ));
            }
            Ok(Self::parse_atom_entries(&doc, effective_base))
        } else if root_name.eq_ignore_ascii_case("rss") || root_name.eq_ignore_ascii_case("rdf") {
            if format == FeedFormat::Atom {
                return Err(FsmError::upstream(
                    KIND,
                    Some(status),
                    "expected Atom feed (dialect.format=atom) but received RSS",
                ));
            }
            Ok(Self::parse_rss_items(&doc, effective_base))
        } else {
            Err(FsmError::upstream(
                KIND,
                Some(status),
                format!("unexpected feed root element <{root_name}> (expected <rss> or <feed>)"),
            ))
        }
    }

    fn filter_entries(entries: Vec<FeedEntry>, query: Option<&str>, limit: usize) -> Vec<Value> {
        let Some(q) = query.map(str::trim).filter(|s| !s.is_empty()) else {
            return entries
                .into_iter()
                .take(limit)
                .map(|e| hit_to_value(e.title, e.url, e.snippet, e.date, Some("feed"), None))
                .collect();
        };

        let q_lower = q.to_lowercase();
        let terms = tokenize_terms(q);
        if terms.is_empty() {
            return entries
                .into_iter()
                .take(limit)
                .map(|e| hit_to_value(e.title, e.url, e.snippet, e.date, Some("feed"), None))
                .collect();
        }

        let mut scored: Vec<(usize, usize, FeedEntry)> = entries
            .into_iter()
            .enumerate()
            .filter_map(|(idx, entry)| {
                let title_l = entry.title.to_lowercase();
                let summary_l = entry.match_summary.to_lowercase();
                let title_words = word_set(&title_l);
                let summary_words = word_set(&summary_l);

                let matched_all = terms.iter().all(|t| {
                    matches_term(&title_l, &title_words, t)
                        || matches_term(&summary_l, &summary_words, t)
                });
                if !matched_all {
                    return None;
                }
                let mut score = 1usize;
                if title_l.contains(&q_lower) {
                    score += 20;
                }
                for t in &terms {
                    if matches_term(&title_l, &title_words, t) {
                        score += 5;
                    }
                }
                Some((score, idx, entry))
            })
            .collect();

        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        scored
            .into_iter()
            .take(limit)
            .map(|(_, _, e)| hit_to_value(e.title, e.url, e.snippet, e.date, Some("feed"), None))
            .collect()
    }
}

#[async_trait::async_trait]
impl ProtocolAdapter for RssAtomAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn ops(&self) -> &[OpDecl] {
        &OPS
    }

    fn validate_dialect(&self, dialect: &BTreeMap<String, Value>) -> Result<(), FsmError> {
        check_allowed_dialect_keys(KIND, dialect, &["format", "max_response_bytes"])?;
        Self::parse_format(dialect)?;
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
        parse_base_url(self.net.as_ref(), base_url)?;
        let format = Self::parse_format(&cx.dialect)?;
        let max_bytes = Self::parse_max_bytes(&cx.dialect)?;
        let map = params_map(&params)?;

        let (query, limit) = match op {
            "search" => (opt_str(map, "query")?, opt_limit(map, 10, 1, 50)?),
            "latest" => (None, opt_limit(map, 10, 1, 50)?),
            _ => return Err(unknown_op(KIND, op, &["search", "latest"])),
        };

        let req = HttpRequest::get(base_url.trim())
            .header("Accept", Self::accept_header(format, base_url))
            .with_max_bytes(max_bytes);
        let resp = send_request(self.net.as_ref(), req, cx, KIND).await?;
        let xml = decode_xml_body(&resp);
        let entries = Self::parse_feed(&xml, base_url.trim(), format, resp.status)?;

        let mut out = ProtocolOutput::new(KIND, base_url, op);
        out.items = Self::filter_entries(entries, query.as_deref(), limit);
        Ok(out)
    }
}
