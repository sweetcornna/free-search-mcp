//! ActivityPub / Fediverse protocol adapter (`activitypub_fediverse`, A1 §E #16).
//!
//! Routes across Lemmy (`/api/v3/search`), Mastodon (`/api/v1/timelines/tag/{tag}`),
//! and PeerTube (`/api/v1/search/videos`), either via explicit `dialect.flavor` or
//! via cached `/.well-known/nodeinfo` discovery.

use crate::util::{
    check_allowed_dialect_keys, clean_snippet, dialect_str, encode_segment, hit_to_value,
    norm_date, opt_limit, opt_str, params_map, parse_base_url, parse_json_body, req_str,
    resolve_http_url, same_registrable_host, send_request, strip_tags, unknown_op,
};
use fsm_core::text::smart_truncate;
use fsm_core::{
    FsmError, HttpRequest, NetClient, OpDecl, Param, ParamType, ProtocolAdapter, ProtocolOutput,
    ReturnKind, SrcCx,
};
use moka::future::Cache;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

const KIND: &str = "activitypub_fediverse";
const VALID_LEMMY_TYPES: &[&str] = &["Posts", "Comments", "Communities", "All"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Auto,
    Lemmy,
    Mastodon,
    PeerTube,
}

static OPS: LazyLock<Vec<OpDecl>> = LazyLock::new(|| {
    vec![
        OpDecl::new(
            "search",
            "Search a Fediverse server (Lemmy posts, Mastodon hashtag timeline, or PeerTube videos).",
            ReturnKind::Hits,
        )
        .with_param(Param::new("query", ParamType::String, "Search terms or hashtag").required())
        .with_param(
            Param::new("limit", ParamType::Int, "Max hits returned")
                .with_default(json!(10))
                .with_range(1.0, 40.0),
        )
        .with_param(
            Param::new(
                "type",
                ParamType::String,
                "Optional Lemmy search type override (Posts|Comments|Communities|All)",
            )
            .with_enum(VALID_LEMMY_TYPES),
        ),
        OpDecl::new(
            "nodeinfo",
            "Discover Fediverse server software and metadata via /.well-known/nodeinfo.",
            ReturnKind::Records,
        ),
    ]
});

pub struct ActivityPubFediverseAdapter {
    net: Arc<dyn NetClient>,
    nodeinfo_cache: Cache<String, String>,
}

impl ActivityPubFediverseAdapter {
    pub fn new(net: Arc<dyn NetClient>) -> Self {
        let nodeinfo_cache = Cache::builder()
            .max_capacity(512)
            .time_to_live(Duration::from_secs(3600))
            .build();
        Self {
            net,
            nodeinfo_cache,
        }
    }

    fn parse_flavor(dialect: &BTreeMap<String, Value>) -> Result<Flavor, FsmError> {
        match dialect_str(KIND, dialect, "flavor")? {
            None | Some("auto") => Ok(Flavor::Auto),
            Some("lemmy") => Ok(Flavor::Lemmy),
            Some("mastodon") => Ok(Flavor::Mastodon),
            Some("peertube") => Ok(Flavor::PeerTube),
            Some(other) => Err(FsmError::invalid(format!(
                "activitypub_fediverse dialect.flavor must be auto|lemmy|mastodon|peertube (got {other:?})"
            ))),
        }
    }

    fn parse_lemmy_type(dialect: &BTreeMap<String, Value>) -> Result<&'static str, FsmError> {
        let raw = dialect_str(KIND, dialect, "lemmy_type")?.unwrap_or("Posts");
        Self::validate_lemmy_type(raw)
    }

    fn validate_lemmy_type(raw: &str) -> Result<&'static str, FsmError> {
        match raw {
            "Posts" => Ok("Posts"),
            "Comments" => Ok("Comments"),
            "Communities" => Ok("Communities"),
            "All" => Ok("All"),
            other => Err(FsmError::invalid(format!(
                "invalid lemmy type {other:?} (expected Posts|Comments|Communities|All)"
            ))),
        }
    }

    fn software_to_flavor(software: &str) -> Result<Flavor, FsmError> {
        match software.trim().to_ascii_lowercase().as_str() {
            "lemmy" => Ok(Flavor::Lemmy),
            "peertube" => Ok(Flavor::PeerTube),
            "mastodon" | "pleroma" | "akkoma" | "gotosocial" | "hometown" | "glitch-soc"
            | "sharkey" | "firefish" | "iceshrimp" => Ok(Flavor::Mastodon),
            other => Err(FsmError::upstream(
                KIND,
                None,
                format!(
                    "unsupported Fediverse software {other:?} (supported: lemmy, mastodon, peertube)"
                ),
            )),
        }
    }

    async fn fetch_nodeinfo(&self, base_no_slash: &str, cx: &SrcCx) -> Result<Value, FsmError> {
        let wk_req = HttpRequest::get(format!("{base_no_slash}/.well-known/nodeinfo"))
            .header("Accept", "application/json");
        let wk_resp = send_request(self.net.as_ref(), wk_req, cx, KIND).await?;
        let wk_json = parse_json_body(&wk_resp, KIND)?;
        let links = wk_json
            .get("links")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                FsmError::upstream(
                    KIND,
                    Some(wk_resp.status),
                    "/.well-known/nodeinfo missing 'links' array",
                )
            })?;

        let raw_href = links
            .iter()
            .find(|l| {
                l.get("rel")
                    .and_then(Value::as_str)
                    .is_some_and(|r| r.contains("2.1"))
            })
            .or_else(|| {
                links.iter().find(|l| {
                    l.get("rel")
                        .and_then(Value::as_str)
                        .is_some_and(|r| r.contains("2.0"))
                })
            })
            .or_else(|| links.first())
            .and_then(|l| l.get("href").and_then(Value::as_str))
            .ok_or_else(|| {
                FsmError::upstream(
                    KIND,
                    Some(wk_resp.status),
                    "/.well-known/nodeinfo had no link with href",
                )
            })?;

        let resolved_href =
            resolve_http_url(&format!("{base_no_slash}/"), raw_href).ok_or_else(|| {
                FsmError::upstream(
                    KIND,
                    Some(wk_resp.status),
                    format!("/.well-known/nodeinfo href {raw_href:?} is not a valid http(s) URL"),
                )
            })?;
        if !same_registrable_host(base_no_slash, &resolved_href) {
            return Err(FsmError::UnsafeUrl(format!(
                "/.well-known/nodeinfo href {resolved_href:?} points outside base host {base_no_slash:?}"
            )));
        }

        let ni_req = HttpRequest::get(resolved_href).header("Accept", "application/json");
        let ni_resp = send_request(self.net.as_ref(), ni_req, cx, KIND).await?;
        parse_json_body(&ni_resp, KIND)
    }

    async fn resolve_flavor(
        &self,
        configured: Flavor,
        base_no_slash: &str,
        cx: &SrcCx,
    ) -> Result<Flavor, FsmError> {
        if configured != Flavor::Auto {
            return Ok(configured);
        }
        if !cx.replay_mode
            && let Some(cached) = self.nodeinfo_cache.get(base_no_slash).await
        {
            return Self::software_to_flavor(&cached);
        }
        let info = self.fetch_nodeinfo(base_no_slash, cx).await?;
        let software = info
            .get("software")
            .and_then(|s| s.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if !cx.replay_mode && !software.is_empty() {
            self.nodeinfo_cache
                .insert(base_no_slash.to_owned(), software.clone())
                .await;
        }
        Self::software_to_flavor(&software)
    }

    async fn search_lemmy(
        &self,
        base_no_slash: &str,
        query: &str,
        limit: usize,
        lemmy_type: &str,
        cx: &SrcCx,
    ) -> Result<Vec<Value>, FsmError> {
        let req = HttpRequest::get(format!("{base_no_slash}/api/v3/search"))
            .header("Accept", "application/json")
            .query_param("q", query)
            .query_param("type_", lemmy_type)
            .query_param("limit", limit.to_string());
        let resp = send_request(self.net.as_ref(), req, cx, KIND).await?;
        let body = parse_json_body(&resp, KIND)?;

        let mut items = Vec::new();

        if let Some(posts) = body.get("posts").and_then(Value::as_array) {
            for wrap in posts {
                if items.len() >= limit {
                    break;
                }
                let post = wrap.get("post").unwrap_or(&Value::Null);
                let title = post
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_owned();
                let post_id = post.get("id").and_then(Value::as_i64);
                let url = post
                    .get("ap_id")
                    .and_then(Value::as_str)
                    .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
                    .map(str::to_owned)
                    .or_else(|| post_id.map(|id| format!("{base_no_slash}/post/{id}")))
                    .unwrap_or_default();
                let link_url = post
                    .get("url")
                    .and_then(Value::as_str)
                    .filter(|u| u.starts_with("http://") || u.starts_with("https://"));
                let raw_body = post.get("body").and_then(Value::as_str).unwrap_or("");
                let community = wrap
                    .get("community")
                    .and_then(|c| c.get("name"))
                    .and_then(Value::as_str);
                let score = wrap
                    .get("counts")
                    .and_then(|c| c.get("score"))
                    .and_then(Value::as_i64);
                let snippet = if !raw_body.trim().is_empty() {
                    clean_snippet(raw_body, 400)
                } else {
                    match (community, score) {
                        (Some(c), Some(s)) => format!("c/{c} · score {s}"),
                        (Some(c), None) => format!("c/{c}"),
                        _ => String::new(),
                    }
                };
                let date = norm_date(post.get("published").and_then(Value::as_str));
                let mut extra_map = serde_json::Map::new();
                extra_map.insert("flavor".into(), json!("lemmy"));
                extra_map.insert("postId".into(), json!(post_id));
                extra_map.insert("community".into(), json!(community));
                extra_map.insert("score".into(), json!(score));
                extra_map.insert(
                    "comments".into(),
                    wrap.get("counts")
                        .and_then(|c| c.get("comments"))
                        .cloned()
                        .unwrap_or(Value::Null),
                );
                if let Some(lu) = link_url {
                    extra_map.insert("linkUrl".into(), json!(lu));
                }
                let extra = Value::Object(extra_map);
                items.push(hit_to_value(
                    title,
                    url,
                    snippet,
                    date,
                    Some("api"),
                    Some(extra),
                ));
            }
        }

        if items.len() < limit
            && let Some(comments) = body.get("comments").and_then(Value::as_array)
        {
            for wrap in comments {
                if items.len() >= limit {
                    break;
                }
                let comment = wrap.get("comment").unwrap_or(&Value::Null);
                let post = wrap.get("post").unwrap_or(&Value::Null);
                let title = post
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("Comment")
                    .to_owned();
                let cid = comment.get("id").and_then(Value::as_i64);
                let url = cid
                    .map(|id| format!("{base_no_slash}/comment/{id}"))
                    .or_else(|| {
                        comment
                            .get("ap_id")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_default();
                let snippet = clean_snippet(
                    comment.get("content").and_then(Value::as_str).unwrap_or(""),
                    400,
                );
                let date = norm_date(comment.get("published").and_then(Value::as_str));
                items.push(hit_to_value(
                    title,
                    url,
                    snippet,
                    date,
                    Some("api"),
                    Some(json!({ "flavor": "lemmy", "commentId": cid })),
                ));
            }
        }

        if items.len() < limit
            && let Some(communities) = body.get("communities").and_then(Value::as_array)
        {
            for wrap in communities {
                if items.len() >= limit {
                    break;
                }
                let comm = wrap.get("community").unwrap_or(&Value::Null);
                let name = comm.get("name").and_then(Value::as_str).unwrap_or("");
                let title = comm
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(name)
                    .to_owned();
                let url = comm
                    .get("actor_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("{base_no_slash}/c/{name}"));
                let snippet = clean_snippet(
                    comm.get("description")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                    400,
                );
                let date = norm_date(comm.get("published").and_then(Value::as_str));
                items.push(hit_to_value(
                    title,
                    url,
                    snippet,
                    date,
                    Some("api"),
                    Some(json!({ "flavor": "lemmy", "community": name })),
                ));
            }
        }

        Ok(items)
    }

    async fn search_mastodon(
        &self,
        base_no_slash: &str,
        query: &str,
        limit: usize,
        cx: &SrcCx,
    ) -> Result<Vec<Value>, FsmError> {
        let tag = query
            .trim()
            .trim_start_matches('#')
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_start_matches('#');
        if tag.is_empty() {
            return Err(FsmError::invalid(
                "Mastodon tag search requires a non-empty hashtag/term",
            ));
        }
        let req = HttpRequest::get(format!(
            "{base_no_slash}/api/v1/timelines/tag/{}",
            encode_segment(tag)
        ))
        .header("Accept", "application/json")
        .query_param("limit", limit.to_string());
        let resp = send_request(self.net.as_ref(), req, cx, KIND).await?;
        let body = parse_json_body(&resp, KIND)?;
        let statuses = body.as_array().ok_or_else(|| {
            FsmError::upstream(
                KIND,
                Some(resp.status),
                "Mastodon tag timeline response was not a JSON array",
            )
        })?;

        let items = statuses
            .iter()
            .take(limit)
            .map(|st| {
                let raw_content = st.get("content").and_then(Value::as_str).unwrap_or("");
                let content = strip_tags(raw_content);
                let spoiler = st
                    .get("spoiler_text")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or("");
                let acct = st
                    .get("account")
                    .and_then(|a| a.get("acct").or_else(|| a.get("username")))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let title = if !spoiler.is_empty() {
                    spoiler.to_owned()
                } else if !content.is_empty() {
                    let (head, _) = smart_truncate(&content, 100);
                    if acct.is_empty() {
                        head
                    } else {
                        format!("@{acct}: {head}")
                    }
                } else if !acct.is_empty() {
                    format!("@{acct} (#{tag})")
                } else {
                    format!("#{tag}")
                };
                let url = st
                    .get("url")
                    .and_then(Value::as_str)
                    .or_else(|| st.get("uri").and_then(Value::as_str))
                    .unwrap_or("")
                    .to_owned();
                let snippet = clean_snippet(raw_content, 400);
                let date = norm_date(st.get("created_at").and_then(Value::as_str));
                let extra = json!({
                    "flavor": "mastodon",
                    "id": st.get("id"),
                    "acct": acct,
                    "favouritesCount": st.get("favourites_count"),
                    "reblogsCount": st.get("reblogs_count"),
                    "repliesCount": st.get("replies_count"),
                });
                hit_to_value(title, url, snippet, date, Some("api"), Some(extra))
            })
            .collect();
        Ok(items)
    }

    async fn search_peertube(
        &self,
        base_no_slash: &str,
        query: &str,
        limit: usize,
        cx: &SrcCx,
    ) -> Result<Vec<Value>, FsmError> {
        let req = HttpRequest::get(format!("{base_no_slash}/api/v1/search/videos"))
            .header("Accept", "application/json")
            .query_param("search", query)
            .query_param("count", limit.to_string());
        let resp = send_request(self.net.as_ref(), req, cx, KIND).await?;
        let body = parse_json_body(&resp, KIND)?;
        let rows = body.get("data").and_then(Value::as_array).ok_or_else(|| {
            FsmError::upstream(
                KIND,
                Some(resp.status),
                "PeerTube search response missing 'data' array",
            )
        })?;

        let items = rows
            .iter()
            .take(limit)
            .map(|v| {
                let title = v
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let url = v
                    .get("url")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| {
                        v.get("shortUUID")
                            .or_else(|| v.get("uuid"))
                            .and_then(Value::as_str)
                            .map(|id| format!("{base_no_slash}/w/{id}"))
                    })
                    .unwrap_or_default();
                let raw_desc = v
                    .get("description")
                    .or_else(|| v.get("truncatedDescription"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let snippet = clean_snippet(raw_desc, 400);
                let date = norm_date(v.get("publishedAt").and_then(Value::as_str))
                    .or_else(|| norm_date(v.get("createdAt").and_then(Value::as_str)));
                let extra = json!({
                    "flavor": "peertube",
                    "uuid": v.get("uuid"),
                    "duration": v.get("duration"),
                    "views": v.get("views"),
                    "channel": v.get("channel").and_then(|c| c.get("displayName")),
                });
                hit_to_value(title, url, snippet, date, Some("api"), Some(extra))
            })
            .collect();
        Ok(items)
    }
}

#[async_trait::async_trait]
impl ProtocolAdapter for ActivityPubFediverseAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn ops(&self) -> &[OpDecl] {
        &OPS
    }

    fn validate_dialect(&self, dialect: &BTreeMap<String, Value>) -> Result<(), FsmError> {
        check_allowed_dialect_keys(KIND, dialect, &["flavor", "lemmy_type"])?;
        Self::parse_flavor(dialect)?;
        Self::parse_lemmy_type(dialect)?;
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
        let (_, base_no_slash) = parse_base_url(self.net.as_ref(), base_url)?;
        let map = params_map(&params)?;

        match op {
            "search" => {
                let query = req_str(map, "query")?;
                let limit = opt_limit(map, 10, 1, 40)?;
                let type_override = opt_str(map, "type")?;
                let configured_flavor = Self::parse_flavor(&cx.dialect)?;
                let lemmy_type = match type_override.as_deref() {
                    Some(t) => Self::validate_lemmy_type(t)?,
                    None => Self::parse_lemmy_type(&cx.dialect)?,
                };

                let flavor = self
                    .resolve_flavor(configured_flavor, &base_no_slash, cx)
                    .await?;

                let items = match flavor {
                    Flavor::Lemmy => {
                        self.search_lemmy(&base_no_slash, &query, limit, lemmy_type, cx)
                            .await?
                    }
                    Flavor::Mastodon => {
                        self.search_mastodon(&base_no_slash, &query, limit, cx)
                            .await?
                    }
                    Flavor::PeerTube => {
                        self.search_peertube(&base_no_slash, &query, limit, cx)
                            .await?
                    }
                    Flavor::Auto => unreachable!("resolve_flavor never returns Auto"),
                };

                let mut out = ProtocolOutput::new(KIND, base_url, op);
                out.items = items;
                Ok(out)
            }
            "nodeinfo" => {
                let info = self.fetch_nodeinfo(&base_no_slash, cx).await?;
                let sw = info.get("software").unwrap_or(&Value::Null);
                let record = json!({
                    "software": sw.get("name").and_then(Value::as_str),
                    "version": sw.get("version").and_then(Value::as_str),
                    "protocols": info.get("protocols").cloned().unwrap_or_else(|| json!([])),
                    "openRegistrations": info.get("openRegistrations"),
                    "usage": info.get("usage"),
                    "metadata": info.get("metadata"),
                });
                let mut out = ProtocolOutput::new(KIND, base_url, op);
                out.items = vec![record];
                Ok(out)
            }
            _ => Err(unknown_op(KIND, op, &["search", "nodeinfo"])),
        }
    }
}
