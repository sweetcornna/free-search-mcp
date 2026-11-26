//! OpenDataSoft Explore API v2.1 protocol adapter (`opendatasoft`, A1 §E #12).

use crate::util::{
    check_allowed_dialect_keys, clean_snippet, dialect_str, encode_segment, norm_date,
    opt_int_range, opt_limit, opt_str, params_map, parse_base_url, parse_json_body, req_str,
    send_request, unknown_op, validate_path_segment,
};
use fsm_core::{
    FsmError, HttpRequest, NetClient, OpDecl, Param, ParamType, ProtocolAdapter, ProtocolOutput,
    ReturnKind, SrcCx,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

const KIND: &str = "opendatasoft";
const DEFAULT_API_VERSION: &str = "v2.1";

static OPS: LazyLock<Vec<OpDecl>> = LazyLock::new(|| {
    vec![
        OpDecl::new(
            "search",
            "Search datasets in an OpenDataSoft Explore catalog.",
            ReturnKind::Records,
        )
        .with_param(Param::new("query", ParamType::String, "Search query").required())
        .with_param(
            Param::new("limit", ParamType::Int, "Max datasets returned")
                .with_default(json!(10))
                .with_range(1.0, 50.0),
        )
        .with_param(
            Param::new("offset", ParamType::Int, "Result offset (min 0)").with_range(0.0, 10_000.0),
        ),
        OpDecl::new(
            "records",
            "Query records inside a specific OpenDataSoft dataset.",
            ReturnKind::Records,
        )
        .with_param(Param::new("dataset_id", ParamType::String, "Dataset identifier").required())
        .with_param(Param::new(
            "query",
            ParamType::String,
            "Optional full-text query",
        ))
        .with_param(
            Param::new("limit", ParamType::Int, "Max records returned")
                .with_default(json!(20))
                .with_range(1.0, 100.0),
        )
        .with_param(
            Param::new("offset", ParamType::Int, "Record offset (min 0)").with_range(0.0, 10_000.0),
        )
        .with_param(Param::new(
            "where",
            ParamType::String,
            "Optional ODSQL where filter",
        )),
    ]
});

pub struct OpenDataSoftAdapter {
    net: Arc<dyn NetClient>,
}

impl OpenDataSoftAdapter {
    pub fn new(net: Arc<dyn NetClient>) -> Self {
        Self { net }
    }

    fn api_version(dialect: &BTreeMap<String, Value>) -> Result<String, FsmError> {
        let raw = dialect_str(KIND, dialect, "api_version")?.unwrap_or(DEFAULT_API_VERSION);
        validate_path_segment("dialect.api_version", raw)
    }

    fn escape_odsql(query: &str) -> String {
        let mut out = String::with_capacity(query.len());
        for ch in query.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                _ => out.push(ch),
            }
        }
        out
    }

    fn project_dataset(entry: &Value, base_no_slash: &str) -> Value {
        let dataset_id = entry
            .get("dataset_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let default_meta = entry
            .get("metas")
            .and_then(|m| m.get("default"))
            .unwrap_or(&Value::Null);

        let title = default_meta
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(dataset_id);

        let url = format!(
            "{base_no_slash}/explore/dataset/{}/",
            encode_segment(dataset_id)
        );

        let desc = default_meta
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let snippet = clean_snippet(desc, 400);

        let date = norm_date(default_meta.get("modified").and_then(Value::as_str))
            .or_else(|| norm_date(default_meta.get("data_processed").and_then(Value::as_str)))
            .or_else(|| {
                norm_date(
                    default_meta
                        .get("metadata_processed")
                        .and_then(Value::as_str),
                )
            });

        let publisher = default_meta
            .get("publisher")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let records_count = default_meta.get("records_count").and_then(Value::as_u64);

        json!({
            "datasetId": dataset_id,
            "title": title,
            "url": url,
            "snippet": snippet,
            "date": date,
            "publisher": publisher,
            "recordsCount": records_count,
        })
    }
}

#[async_trait::async_trait]
impl ProtocolAdapter for OpenDataSoftAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn ops(&self) -> &[OpDecl] {
        &OPS
    }

    fn validate_dialect(&self, dialect: &BTreeMap<String, Value>) -> Result<(), FsmError> {
        check_allowed_dialect_keys(KIND, dialect, &["api_version"])?;
        Self::api_version(dialect)?;
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
        let version = Self::api_version(&cx.dialect)?;
        let map = params_map(&params)?;

        match op {
            "search" => {
                let query = req_str(map, "query")?;
                let limit = opt_limit(map, 10, 1, 50)?;
                let offset = opt_int_range(map, "offset", 0, 10_000)?;
                let where_expr = format!("search(*,\"{}\")", Self::escape_odsql(&query));

                let mut req = HttpRequest::get(format!(
                    "{base_no_slash}/api/explore/{version}/catalog/datasets"
                ))
                .header("Accept", "application/json")
                .query_param("where", where_expr)
                .query_param("limit", limit.to_string());
                if let Some(off) = offset {
                    req = req.query_param("offset", off.to_string());
                }

                let resp = send_request(self.net.as_ref(), req, cx, KIND).await?;
                let body = parse_json_body(&resp, KIND)?;
                let results = body
                    .get("results")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        FsmError::upstream(
                            KIND,
                            Some(resp.status),
                            "OpenDataSoft catalog response missing 'results' array",
                        )
                    })?;

                let items: Vec<Value> = results
                    .iter()
                    .take(limit)
                    .map(|r| Self::project_dataset(r, &base_no_slash))
                    .collect();

                let total = body.get("total_count").and_then(Value::as_u64).unwrap_or(0);
                let next_pos = (offset.unwrap_or(0) as u64).saturating_add(items.len() as u64);

                let mut out = ProtocolOutput::new(KIND, base_url, op);
                out.items = items;
                if !out.items.is_empty() && next_pos < total {
                    out.next_cursor = Some(next_pos.to_string());
                }
                Ok(out)
            }
            "records" => {
                let raw_id = req_str(map, "dataset_id")?;
                let dataset_id = validate_path_segment("dataset_id", &raw_id)?;
                let query = opt_str(map, "query")?;
                let limit = opt_limit(map, 20, 1, 100)?;
                let offset = opt_int_range(map, "offset", 0, 10_000)?;
                let where_param = opt_str(map, "where")?;

                let effective_where = match (where_param, query) {
                    (Some(w), Some(q)) => Some(format!(
                        "({w}) AND search(*,\"{}\")",
                        Self::escape_odsql(&q)
                    )),
                    (Some(w), None) => Some(w),
                    (None, Some(q)) => Some(format!("search(*,\"{}\")", Self::escape_odsql(&q))),
                    (None, None) => None,
                };

                let mut req = HttpRequest::get(format!(
                    "{base_no_slash}/api/explore/{version}/catalog/datasets/{}/records",
                    encode_segment(&dataset_id)
                ))
                .header("Accept", "application/json")
                .query_param("limit", limit.to_string());
                if let Some(off) = offset {
                    req = req.query_param("offset", off.to_string());
                }
                if let Some(w) = effective_where {
                    req = req.query_param("where", w);
                }

                let resp = send_request(self.net.as_ref(), req, cx, KIND).await?;
                let body = parse_json_body(&resp, KIND)?;
                let results = body
                    .get("results")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        FsmError::upstream(
                            KIND,
                            Some(resp.status),
                            "OpenDataSoft records response missing 'results' array",
                        )
                    })?;

                let total = body.get("total_count").and_then(Value::as_u64).unwrap_or(0);
                let items: Vec<Value> = results.iter().take(limit).cloned().collect();
                let next_pos = (offset.unwrap_or(0) as u64).saturating_add(items.len() as u64);

                let mut out = ProtocolOutput::new(KIND, base_url, op);
                out.items = items;
                if !out.items.is_empty() && next_pos < total {
                    out.next_cursor = Some(next_pos.to_string());
                }
                Ok(out)
            }
            _ => Err(unknown_op(KIND, op, &["search", "records"])),
        }
    }
}
