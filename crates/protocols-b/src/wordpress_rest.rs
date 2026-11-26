//! WordPress REST API v2 protocol adapter (`wordpress_rest`, A1 §E #14).

use crate::util::{
    check_allowed_dialect_keys, clean_snippet, collapse_ws, dialect_str, hit_to_value, norm_date,
    opt_int_range, opt_limit, opt_str, params_map, parse_base_url, parse_json_body, req_int,
    req_str, send_request, strip_tags, unescape_html, unknown_op, validate_subpath,
};
use fsm_core::{
    FsmError, HttpRequest, NetClient, OpDecl, Param, ParamType, ProtocolAdapter, ProtocolOutput,
    ReturnKind, SrcCx,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

const KIND: &str = "wordpress_rest";
const DEFAULT_REST_BASE: &str = "wp-json";

static OPS: LazyLock<Vec<OpDecl>> = LazyLock::new(|| {
    vec![
        OpDecl::new(
            "search",
            "Search posts/pages via WordPress REST API v2 (/wp-json/wp/v2/search).",
            ReturnKind::Hits,
        )
        .with_param(Param::new("query", ParamType::String, "Search terms").required())
        .with_param(
            Param::new("limit", ParamType::Int, "Max hits per page")
                .with_default(json!(10))
                .with_range(1.0, 50.0),
        )
        .with_param(
            Param::new("page", ParamType::Int, "Optional 1-based page").with_range(1.0, 100.0),
        )
        .with_param(Param::new(
            "subtype",
            ParamType::String,
            "Optional WordPress content subtype (e.g. post, page)",
        )),
        OpDecl::new(
            "post",
            "Fetch a WordPress post by numeric id (/wp-json/wp/v2/posts/{id}).",
            ReturnKind::Records,
        )
        .with_param(Param::new("id", ParamType::Int, "WordPress post ID").required()),
    ]
});

pub struct WordPressRestAdapter {
    net: Arc<dyn NetClient>,
}

impl WordPressRestAdapter {
    pub fn new(net: Arc<dyn NetClient>) -> Self {
        Self { net }
    }

    fn rest_base(dialect: &BTreeMap<String, Value>) -> Result<String, FsmError> {
        let raw = dialect_str(KIND, dialect, "rest_base")?.unwrap_or(DEFAULT_REST_BASE);
        validate_subpath("dialect.rest_base", raw)
    }
}

#[async_trait::async_trait]
impl ProtocolAdapter for WordPressRestAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn ops(&self) -> &[OpDecl] {
        &OPS
    }

    fn validate_dialect(&self, dialect: &BTreeMap<String, Value>) -> Result<(), FsmError> {
        check_allowed_dialect_keys(KIND, dialect, &["rest_base"])?;
        Self::rest_base(dialect)?;
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
        let rest_base = Self::rest_base(&cx.dialect)?;
        let map = params_map(&params)?;

        match op {
            "search" => {
                let query = req_str(map, "query")?;
                let limit = opt_limit(map, 10, 1, 50)?;
                let page = opt_int_range(map, "page", 1, 100)?;
                let subtype = opt_str(map, "subtype")?;

                let mut req = HttpRequest::get(format!("{base_no_slash}/{rest_base}/wp/v2/search"))
                    .header("Accept", "application/json")
                    .query_param("search", query)
                    .query_param("per_page", limit.to_string());
                if let Some(p) = page {
                    req = req.query_param("page", p.to_string());
                }
                if let Some(st) = subtype {
                    req = req.query_param("subtype", st);
                }

                let resp = send_request(self.net.as_ref(), req, cx, KIND).await?;
                let total_pages = resp
                    .header("x-wp-totalpages")
                    .and_then(|v| v.trim().parse::<i64>().ok());
                let body = parse_json_body(&resp, KIND)?;
                let rows = body.as_array().ok_or_else(|| {
                    FsmError::upstream(
                        KIND,
                        Some(resp.status),
                        "WordPress REST /search response was not a JSON array",
                    )
                })?;

                let items: Vec<Value> = rows
                    .iter()
                    .take(limit)
                    .map(|row| {
                        let raw_title = row.get("title").and_then(Value::as_str).unwrap_or("");
                        let title = collapse_ws(&unescape_html(raw_title));
                        let url = row
                            .get("url")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        let extra = json!({
                            "id": row.get("id"),
                            "type": row.get("type"),
                            "subtype": row.get("subtype"),
                        });
                        hit_to_value(title, url, String::new(), None, None, Some(extra))
                    })
                    .collect();

                let cur_page = page.unwrap_or(1);
                let mut out = ProtocolOutput::new(KIND, base_url, op);
                out.items = items;
                if let Some(tp) = total_pages
                    && cur_page < tp.min(100)
                {
                    out.next_cursor = Some(cur_page.saturating_add(1).to_string());
                }
                Ok(out)
            }
            "post" => {
                let id = req_int(map, "id", 1)?;
                let req = HttpRequest::get(format!("{base_no_slash}/{rest_base}/wp/v2/posts/{id}"))
                    .header("Accept", "application/json");
                let resp = send_request(self.net.as_ref(), req, cx, KIND).await?;
                let body = parse_json_body(&resp, KIND)?;

                let title_raw = body
                    .get("title")
                    .and_then(|t| t.get("rendered"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let excerpt_raw = body
                    .get("excerpt")
                    .and_then(|e| e.get("rendered"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let content_raw = body
                    .get("content")
                    .and_then(|c| c.get("rendered"))
                    .and_then(Value::as_str)
                    .unwrap_or("");

                let record = json!({
                    "id": body.get("id").and_then(Value::as_i64).unwrap_or(id),
                    "title": strip_tags(title_raw),
                    "url": body.get("link").and_then(Value::as_str).unwrap_or(""),
                    "slug": body.get("slug").and_then(Value::as_str),
                    "status": body.get("status").and_then(Value::as_str),
                    "date": norm_date(body.get("date_gmt").and_then(Value::as_str))
                        .or_else(|| norm_date(body.get("date").and_then(Value::as_str))),
                    "snippet": clean_snippet(excerpt_raw, 400),
                    "content": clean_snippet(content_raw, 4000),
                });

                let mut out = ProtocolOutput::new(KIND, base_url, op);
                out.items = vec![record];
                Ok(out)
            }
            _ => Err(unknown_op(KIND, op, &["search", "post"])),
        }
    }
}
