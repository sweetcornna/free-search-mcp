//! Discourse forum JSON protocol adapter (`discourse`, A1 §E #13).

use crate::util::{
    check_allowed_dialect_keys, clean_plain_text, clean_snippet, dialect_str, hit_to_value,
    norm_date, opt_int_range, opt_limit, opt_str, params_map, parse_base_url, parse_json_body,
    req_int, req_str, send_request, unescape_html, unknown_op, validate_subpath,
};
use fsm_core::{
    FsmError, HttpRequest, NetClient, OpDecl, Param, ParamType, ProtocolAdapter, ProtocolOutput,
    ReturnKind, SrcCx,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock};

const KIND: &str = "discourse";

static OPS: LazyLock<Vec<OpDecl>> = LazyLock::new(|| {
    vec![
        OpDecl::new(
            "search",
            "Search posts and topics on a Discourse forum (/search.json).",
            ReturnKind::Hits,
        )
        .with_param(Param::new("query", ParamType::String, "Search query").required())
        .with_param(
            Param::new("limit", ParamType::Int, "Max hits returned")
                .with_default(json!(10))
                .with_range(1.0, 50.0),
        )
        .with_param(
            Param::new("page", ParamType::Int, "Optional 1-based page").with_range(1.0, 100.0),
        ),
        OpDecl::new(
            "topic",
            "Fetch a Discourse topic thread by numeric id (/t/{id}.json).",
            ReturnKind::Records,
        )
        .with_param(Param::new("id", ParamType::Int, "Discourse topic id").required()),
        OpDecl::new(
            "latest",
            "List latest topics on a Discourse forum (/latest.json or /c/{category}.json).",
            ReturnKind::Hits,
        )
        .with_param(
            Param::new("limit", ParamType::Int, "Max topics returned")
                .with_default(json!(20))
                .with_range(1.0, 50.0),
        )
        .with_param(Param::new(
            "category",
            ParamType::String,
            "Optional category slug/path (e.g. 'help' or 'dev/5')",
        )),
    ]
});

pub struct DiscourseAdapter {
    net: Arc<dyn NetClient>,
}

impl DiscourseAdapter {
    pub fn new(net: Arc<dyn NetClient>) -> Self {
        Self { net }
    }

    fn base_path(dialect: &BTreeMap<String, Value>) -> Result<String, FsmError> {
        let Some(raw) = dialect_str(KIND, dialect, "base_path")? else {
            return Ok(String::new());
        };
        let trimmed = raw.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            return Ok(String::new());
        }
        let clean = validate_subpath("dialect.base_path", trimmed)?;
        Ok(format!("/{clean}"))
    }

    fn topic_url(
        base_no_slash: &str,
        base_path: &str,
        slug: Option<&str>,
        topic_id: i64,
        post_number: Option<i64>,
    ) -> String {
        let base = match slug.filter(|s| !s.is_empty()) {
            Some(s) => format!("{base_no_slash}{base_path}/t/{s}/{topic_id}"),
            None => format!("{base_no_slash}{base_path}/t/{topic_id}"),
        };
        match post_number {
            Some(n) if n > 1 => format!("{base}/{n}"),
            _ => base,
        }
    }
}

#[async_trait::async_trait]
impl ProtocolAdapter for DiscourseAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn ops(&self) -> &[OpDecl] {
        &OPS
    }

    fn validate_dialect(&self, dialect: &BTreeMap<String, Value>) -> Result<(), FsmError> {
        check_allowed_dialect_keys(KIND, dialect, &["base_path"])?;
        Self::base_path(dialect)?;
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
        let base_path = Self::base_path(&cx.dialect)?;
        let map = params_map(&params)?;

        match op {
            "search" => {
                let query = req_str(map, "query")?;
                let limit = opt_limit(map, 10, 1, 50)?;
                let page = opt_int_range(map, "page", 1, 100)?;

                let mut req = HttpRequest::get(format!("{base_no_slash}{base_path}/search.json"))
                    .header("Accept", "application/json")
                    .query_param("q", query);
                if let Some(p) = page {
                    req = req.query_param("page", p.to_string());
                }

                let resp = send_request(self.net.as_ref(), req, cx, KIND).await?;
                let body = parse_json_body(&resp, KIND)?;

                let topics_by_id: HashMap<i64, &Value> = body
                    .get("topics")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|t| t.get("id").and_then(Value::as_i64).map(|id| (id, t)))
                            .collect()
                    })
                    .unwrap_or_default();

                let mut items = Vec::new();
                if let Some(posts) = body.get("posts").and_then(Value::as_array) {
                    for post in posts.iter().take(limit) {
                        let topic_id = post.get("topic_id").and_then(Value::as_i64).unwrap_or(0);
                        let topic = topics_by_id.get(&topic_id).copied();
                        let title_raw = topic
                            .and_then(|t| {
                                t.get("title")
                                    .and_then(Value::as_str)
                                    .or_else(|| t.get("fancy_title").and_then(Value::as_str))
                            })
                            .unwrap_or("");
                        let title = unescape_html(title_raw);
                        let slug = topic.and_then(|t| t.get("slug").and_then(Value::as_str));
                        let post_number = post.get("post_number").and_then(Value::as_i64);
                        let url = Self::topic_url(
                            &base_no_slash,
                            &base_path,
                            slug,
                            topic_id,
                            post_number,
                        );
                        let blurb = post.get("blurb").and_then(Value::as_str).unwrap_or("");
                        let snippet = clean_plain_text(blurb, 400);
                        let date = norm_date(post.get("created_at").and_then(Value::as_str))
                            .or_else(|| {
                                topic.and_then(|t| {
                                    norm_date(t.get("last_posted_at").and_then(Value::as_str))
                                })
                            });
                        let extra = json!({
                            "topicId": topic_id,
                            "postId": post.get("id"),
                            "postNumber": post_number,
                            "username": post.get("username"),
                            "likeCount": post.get("like_count"),
                            "postsCount": topic.and_then(|t| t.get("posts_count")),
                        });
                        items.push(hit_to_value(
                            title,
                            url,
                            snippet,
                            date,
                            Some("api"),
                            Some(extra),
                        ));
                    }
                } else if let Some(topics) = body.get("topics").and_then(Value::as_array) {
                    for topic in topics.iter().take(limit) {
                        let topic_id = topic.get("id").and_then(Value::as_i64).unwrap_or(0);
                        let title =
                            unescape_html(topic.get("title").and_then(Value::as_str).unwrap_or(""));
                        let slug = topic.get("slug").and_then(Value::as_str);
                        let url = Self::topic_url(&base_no_slash, &base_path, slug, topic_id, None);
                        let date = norm_date(topic.get("last_posted_at").and_then(Value::as_str))
                            .or_else(|| norm_date(topic.get("created_at").and_then(Value::as_str)));
                        items.push(hit_to_value(
                            title,
                            url,
                            String::new(),
                            date,
                            Some("api"),
                            Some(json!({ "topicId": topic_id })),
                        ));
                    }
                }

                let more = body
                    .get("grouped_search_result")
                    .and_then(|g| g.get("more_full_page_results"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let cur_page = page.unwrap_or(1);
                let mut out = ProtocolOutput::new(KIND, base_url, op);
                out.items = items;
                if more && cur_page < 100 {
                    out.next_cursor = Some(cur_page.saturating_add(1).to_string());
                }
                Ok(out)
            }
            "topic" => {
                let id = req_int(map, "id", 1)?;
                let req = HttpRequest::get(format!("{base_no_slash}{base_path}/t/{id}.json"))
                    .header("Accept", "application/json");
                let resp = send_request(self.net.as_ref(), req, cx, KIND).await?;
                let body = parse_json_body(&resp, KIND)?;

                let title = unescape_html(body.get("title").and_then(Value::as_str).unwrap_or(""));
                let slug = body.get("slug").and_then(Value::as_str);
                let url = Self::topic_url(&base_no_slash, &base_path, slug, id, None);
                let posts: Vec<Value> = body
                    .get("post_stream")
                    .and_then(|ps| ps.get("posts"))
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .take(20)
                            .map(|p| {
                                let cooked = p.get("cooked").and_then(Value::as_str).unwrap_or("");
                                json!({
                                    "id": p.get("id"),
                                    "postNumber": p.get("post_number"),
                                    "username": p.get("username"),
                                    "createdAt": norm_date(p.get("created_at").and_then(Value::as_str)),
                                    "text": clean_snippet(cooked, 1500),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                let record = json!({
                    "id": body.get("id").and_then(Value::as_i64).unwrap_or(id),
                    "title": title,
                    "slug": slug,
                    "url": url,
                    "postsCount": body.get("posts_count"),
                    "views": body.get("views"),
                    "date": norm_date(body.get("last_posted_at").and_then(Value::as_str))
                        .or_else(|| norm_date(body.get("created_at").and_then(Value::as_str))),
                    "tags": body.get("tags").cloned().unwrap_or_else(|| json!([])),
                    "posts": posts,
                });

                let mut out = ProtocolOutput::new(KIND, base_url, op);
                out.items = vec![record];
                Ok(out)
            }
            "latest" => {
                let limit = opt_limit(map, 20, 1, 50)?;
                let category = opt_str(map, "category")?;
                let endpoint = match category {
                    Some(cat) => {
                        let safe = validate_subpath("category", &cat)?;
                        format!("{base_no_slash}{base_path}/c/{safe}.json")
                    }
                    None => format!("{base_no_slash}{base_path}/latest.json"),
                };
                let req = HttpRequest::get(endpoint).header("Accept", "application/json");
                let resp = send_request(self.net.as_ref(), req, cx, KIND).await?;
                let body = parse_json_body(&resp, KIND)?;

                let topics = body
                    .get("topic_list")
                    .and_then(|tl| tl.get("topics"))
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        FsmError::upstream(
                            KIND,
                            Some(resp.status),
                            "Discourse latest response missing topic_list.topics array",
                        )
                    })?;

                let items: Vec<Value> = topics
                    .iter()
                    .take(limit)
                    .map(|t| {
                        let tid = t.get("id").and_then(Value::as_i64).unwrap_or(0);
                        let title =
                            unescape_html(t.get("title").and_then(Value::as_str).unwrap_or(""));
                        let slug = t.get("slug").and_then(Value::as_str);
                        let url = Self::topic_url(&base_no_slash, &base_path, slug, tid, None);
                        let excerpt = t.get("excerpt").and_then(Value::as_str).unwrap_or("");
                        let snippet = clean_snippet(excerpt, 400);
                        let date = norm_date(t.get("last_posted_at").and_then(Value::as_str))
                            .or_else(|| norm_date(t.get("created_at").and_then(Value::as_str)));
                        let extra = json!({
                            "topicId": tid,
                            "postsCount": t.get("posts_count"),
                            "views": t.get("views"),
                            "likeCount": t.get("like_count"),
                            "tags": t.get("tags"),
                        });
                        hit_to_value(title, url, snippet, date, Some("api"), Some(extra))
                    })
                    .collect();

                let mut out = ProtocolOutput::new(KIND, base_url, op);
                out.items = items;
                Ok(out)
            }
            _ => Err(unknown_op(KIND, op, &["search", "topic", "latest"])),
        }
    }
}
