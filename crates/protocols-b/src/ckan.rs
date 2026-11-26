//! CKAN Action API v3 protocol adapter (`ckan`, A1 §E #10).

use crate::util::{
    check_allowed_dialect_keys, clean_snippet, dialect_str, encode_segment, multilingual_text,
    norm_date, opt_int_range, opt_limit, opt_str, params_map, parse_base_url, parse_json_body,
    req_str, send_request, unknown_op,
};
use fsm_core::{
    FsmError, HttpRequest, NetClient, OpDecl, Param, ParamType, ProtocolAdapter, ProtocolOutput,
    ReturnKind, SrcCx,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

const KIND: &str = "ckan";
const DEFAULT_API_PREFIX: &str = "/api/3/action";

static OPS: LazyLock<Vec<OpDecl>> = LazyLock::new(|| {
    vec![
        OpDecl::new(
            "search",
            "Search datasets in a CKAN open-data catalog (package_search).",
            ReturnKind::Records,
        )
        .with_param(Param::new("query", ParamType::String, "Search query").required())
        .with_param(
            Param::new("limit", ParamType::Int, "Max datasets returned")
                .with_default(json!(10))
                .with_range(1.0, 50.0),
        )
        .with_param(
            Param::new("offset", ParamType::Int, "Start offset (min 0)").with_range(0.0, 10_000.0),
        )
        .with_param(Param::new(
            "fq",
            ParamType::String,
            "Optional Solr filter query",
        ))
        .with_param(Param::new(
            "sort",
            ParamType::String,
            "Optional CKAN sort expression",
        )),
        OpDecl::new(
            "package",
            "Fetch full metadata for a CKAN dataset package by id or name (package_show).",
            ReturnKind::Records,
        )
        .with_param(Param::new("id", ParamType::String, "Dataset package id or slug").required()),
    ]
});

pub struct CkanAdapter {
    net: Arc<dyn NetClient>,
}

impl CkanAdapter {
    pub fn new(net: Arc<dyn NetClient>) -> Self {
        Self { net }
    }

    fn api_prefix(dialect: &BTreeMap<String, Value>) -> Result<String, FsmError> {
        let raw = dialect_str(KIND, dialect, "api_prefix")?.unwrap_or(DEFAULT_API_PREFIX);
        let trimmed = raw.trim().trim_end_matches('/');
        if trimmed.is_empty() || trimmed.contains("..") {
            return Err(FsmError::invalid(
                "ckan dialect.api_prefix must be a non-empty path prefix",
            ));
        }
        if trimmed.starts_with('/') {
            Ok(trimmed.to_owned())
        } else {
            Ok(format!("/{trimmed}"))
        }
    }

    fn project_package(pkg: &Value, base_no_slash: &str, prefer_lang: Option<&str>) -> Value {
        let id = pkg.get("id").and_then(Value::as_str).unwrap_or("");
        let name = pkg.get("name").and_then(Value::as_str).unwrap_or(id);
        let title = multilingual_text(pkg.get("title"), prefer_lang)
            .or_else(|| multilingual_text(pkg.get("title_translated"), prefer_lang))
            .unwrap_or_else(|| name.to_owned());

        let url = format!("{base_no_slash}/dataset/{}", encode_segment(name));
        let source_url = pkg
            .get("url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
            .map(str::to_owned);

        let raw_notes = multilingual_text(pkg.get("notes"), prefer_lang)
            .or_else(|| multilingual_text(pkg.get("description"), prefer_lang))
            .or_else(|| multilingual_text(pkg.get("notes_translated"), prefer_lang))
            .unwrap_or_default();
        let snippet = clean_snippet(&raw_notes, 400);

        let date = norm_date(pkg.get("metadata_modified").and_then(Value::as_str))
            .or_else(|| norm_date(pkg.get("modified").and_then(Value::as_str)))
            .or_else(|| norm_date(pkg.get("metadata_created").and_then(Value::as_str)))
            .or_else(|| norm_date(pkg.get("issued").and_then(Value::as_str)));

        let organization = pkg.get("organization").and_then(|org| {
            multilingual_text(org.get("title"), prefer_lang)
                .or_else(|| multilingual_text(org.get("display_name"), prefer_lang))
                .or_else(|| org.get("name").and_then(Value::as_str).map(str::to_owned))
        });

        let license = pkg
            .get("license_title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                pkg.get("license_id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            })
            .map(str::to_owned);

        let resources: Vec<Value> = pkg
            .get("resources")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .take(20)
                    .map(|r| {
                        let r_name = multilingual_text(r.get("name"), prefer_lang)
                            .or_else(|| multilingual_text(r.get("description"), prefer_lang));
                        json!({
                            "id": r.get("id").and_then(Value::as_str),
                            "name": r_name,
                            "format": r.get("format").and_then(Value::as_str),
                            "url": r.get("url").and_then(Value::as_str),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut obj = json!({
            "id": id,
            "name": name,
            "title": title,
            "url": url,
            "snippet": snippet,
            "date": date,
            "organization": organization,
            "license": license,
            "resources": resources,
        });
        if let Some(src_url) = source_url
            && let Some(m) = obj.as_object_mut()
        {
            m.insert("sourceUrl".to_owned(), Value::String(src_url));
        }
        obj
    }

    fn check_envelope(body: &Value, status: u16) -> Result<&Value, FsmError> {
        if body.get("success").and_then(Value::as_bool) == Some(false) {
            let msg = body
                .get("error")
                .and_then(|e| {
                    e.get("message")
                        .and_then(Value::as_str)
                        .or_else(|| e.as_str())
                        .map(str::to_owned)
                        .or_else(|| Some(e.to_string()))
                })
                .unwrap_or_else(|| "CKAN API returned success=false".to_owned());
            return Err(FsmError::upstream(KIND, Some(status), msg));
        }
        body.get("result").ok_or_else(|| {
            FsmError::upstream(KIND, Some(status), "CKAN response missing 'result' object")
        })
    }
}

#[async_trait::async_trait]
impl ProtocolAdapter for CkanAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn ops(&self) -> &[OpDecl] {
        &OPS
    }

    fn validate_dialect(&self, dialect: &BTreeMap<String, Value>) -> Result<(), FsmError> {
        check_allowed_dialect_keys(KIND, dialect, &["api_prefix"])?;
        Self::api_prefix(dialect)?;
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
        let api_prefix = Self::api_prefix(&cx.dialect)?;
        let map = params_map(&params)?;
        let prefer_lang = cx.lang.as_deref();

        match op {
            "search" => {
                let query = req_str(map, "query")?;
                let limit = opt_limit(map, 10, 1, 50)?;
                let offset = opt_int_range(map, "offset", 0, 10_000)?;
                let fq = opt_str(map, "fq")?;
                let sort = opt_str(map, "sort")?;

                let mut req =
                    HttpRequest::get(format!("{base_no_slash}{api_prefix}/package_search"))
                        .header("Accept", "application/json")
                        .query_param("q", query)
                        .query_param("rows", limit.to_string());
                if let Some(start) = offset {
                    req = req.query_param("start", start.to_string());
                }
                if let Some(fq_val) = fq {
                    req = req.query_param("fq", fq_val);
                }
                if let Some(sort_val) = sort {
                    req = req.query_param("sort", sort_val);
                }

                let resp = send_request(self.net.as_ref(), req, cx, KIND).await?;
                let body = parse_json_body(&resp, KIND)?;
                let result = Self::check_envelope(&body, resp.status)?;
                let results = result
                    .get("results")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        FsmError::upstream(
                            KIND,
                            Some(resp.status),
                            "CKAN package_search missing result.results array",
                        )
                    })?;

                let items: Vec<Value> = results
                    .iter()
                    .take(limit)
                    .map(|pkg| Self::project_package(pkg, &base_no_slash, prefer_lang))
                    .collect();

                let count = result.get("count").and_then(Value::as_u64).unwrap_or(0);
                let start_pos = offset.unwrap_or(0) as u64;
                let next_pos = start_pos.saturating_add(items.len() as u64);

                let mut out = ProtocolOutput::new(KIND, base_url, op);
                out.items = items;
                if !out.items.is_empty() && next_pos < count {
                    out.next_cursor = Some(next_pos.to_string());
                }
                Ok(out)
            }
            "package" => {
                let id = req_str(map, "id")?;
                let req = HttpRequest::get(format!("{base_no_slash}{api_prefix}/package_show"))
                    .header("Accept", "application/json")
                    .query_param("id", id);
                let resp = send_request(self.net.as_ref(), req, cx, KIND).await?;
                let body = parse_json_body(&resp, KIND)?;
                let pkg = Self::check_envelope(&body, resp.status)?;

                let mut out = ProtocolOutput::new(KIND, base_url, op);
                out.items = vec![Self::project_package(pkg, &base_no_slash, prefer_lang)];
                Ok(out)
            }
            _ => Err(unknown_op(KIND, op, &["search", "package"])),
        }
    }
}
