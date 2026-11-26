//! Socrata Discovery Catalog v1 & SODA row query adapter (`socrata`, A1 §E #11).

use crate::util::{
    check_allowed_dialect_keys, clean_plain_text, dialect_bool, encode_segment, norm_date,
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

const KIND: &str = "socrata";
const GLOBAL_DISCOVERY_HOST: &str = "api.us.socrata.com";

static OPS: LazyLock<Vec<OpDecl>> = LazyLock::new(|| {
    vec![
        OpDecl::new(
            "search",
            "Search datasets in a Socrata open-data catalog (/api/catalog/v1).",
            ReturnKind::Records,
        )
        .with_param(Param::new("query", ParamType::String, "Search terms").required())
        .with_param(
            Param::new("limit", ParamType::Int, "Max datasets returned")
                .with_default(json!(10))
                .with_range(1.0, 50.0),
        )
        .with_param(
            Param::new("offset", ParamType::Int, "Result offset (min 0)").with_range(0.0, 10_000.0),
        )
        .with_param(Param::new(
            "domains",
            ParamType::String,
            "Comma-separated Socrata portal domain filter",
        )),
        OpDecl::new(
            "rows",
            "Query rows from a Socrata dataset via SODA (/resource/{dataset_id}.json).",
            ReturnKind::Records,
        )
        .with_param(
            Param::new(
                "dataset_id",
                ParamType::String,
                "Socrata 4x4 dataset identifier",
            )
            .required(),
        )
        .with_param(
            Param::new("limit", ParamType::Int, "Max rows returned")
                .with_default(json!(20))
                .with_range(1.0, 100.0),
        )
        .with_param(
            Param::new("offset", ParamType::Int, "Row offset (min 0)").with_range(0.0, 100_000.0),
        )
        .with_param(Param::new(
            "where",
            ParamType::String,
            "Optional SoQL $where clause",
        ))
        .with_param(Param::new(
            "q",
            ParamType::String,
            "Optional full-text $q filter",
        )),
    ]
});

pub struct SocrataAdapter {
    net: Arc<dyn NetClient>,
}

impl SocrataAdapter {
    pub fn new(net: Arc<dyn NetClient>) -> Self {
        Self { net }
    }

    fn project_catalog_item(entry: &Value) -> Value {
        let res = entry.get("resource").unwrap_or(&Value::Null);
        let meta = entry.get("metadata").unwrap_or(&Value::Null);
        let id = res.get("id").and_then(Value::as_str).unwrap_or("");
        let title = res.get("name").and_then(Value::as_str).unwrap_or(id);
        let domain = meta.get("domain").and_then(Value::as_str);

        let url = entry
            .get("permalink")
            .and_then(Value::as_str)
            .or_else(|| entry.get("link").and_then(Value::as_str))
            .map(str::to_owned)
            .unwrap_or_else(|| match domain {
                Some(d) if !id.is_empty() => format!("https://{d}/d/{id}"),
                _ => String::new(),
            });

        let desc = res.get("description").and_then(Value::as_str).unwrap_or("");
        let snippet = clean_plain_text(desc, 400);

        let date = norm_date(res.get("updatedAt").and_then(Value::as_str))
            .or_else(|| norm_date(res.get("data_updated_at").and_then(Value::as_str)))
            .or_else(|| norm_date(res.get("metadata_updated_at").and_then(Value::as_str)))
            .or_else(|| norm_date(res.get("createdAt").and_then(Value::as_str)));

        let kind_type = res.get("type").and_then(Value::as_str);

        let columns: Vec<String> = res
            .get("columns_name")
            .and_then(Value::as_array)
            .or_else(|| res.get("columns_field_name").and_then(Value::as_array))
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .take(30)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();

        json!({
            "id": id,
            "title": title,
            "url": url,
            "snippet": snippet,
            "date": date,
            "domain": domain,
            "type": kind_type,
            "columns": columns,
        })
    }
}

#[async_trait::async_trait]
impl ProtocolAdapter for SocrataAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn ops(&self) -> &[OpDecl] {
        &OPS
    }

    fn validate_dialect(&self, dialect: &BTreeMap<String, Value>) -> Result<(), FsmError> {
        check_allowed_dialect_keys(KIND, dialect, &["auto_scope_domain"])?;
        dialect_bool(KIND, dialect, "auto_scope_domain")?;
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
        let (parsed_base, base_no_slash) = parse_base_url(self.net.as_ref(), base_url)?;
        let map = params_map(&params)?;

        match op {
            "search" => {
                let query = req_str(map, "query")?;
                let limit = opt_limit(map, 10, 1, 50)?;
                let offset = opt_int_range(map, "offset", 0, 10_000)?;
                let explicit_domains = opt_str(map, "domains")?;
                let auto_scope =
                    dialect_bool(KIND, &cx.dialect, "auto_scope_domain")?.unwrap_or(true);

                let effective_domains = match explicit_domains {
                    Some(d) => Some(d),
                    None if auto_scope => parsed_base
                        .host_str()
                        .filter(|h| !h.eq_ignore_ascii_case(GLOBAL_DISCOVERY_HOST))
                        .map(str::to_owned),
                    None => None,
                };

                let mut req = HttpRequest::get(format!("{base_no_slash}/api/catalog/v1"))
                    .header("Accept", "application/json")
                    .query_param("q", query)
                    .query_param("limit", limit.to_string());
                if let Some(domains) = effective_domains {
                    req = req.query_param("domains", domains);
                }
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
                            "Socrata catalog response missing 'results' array",
                        )
                    })?;

                let items: Vec<Value> = results
                    .iter()
                    .take(limit)
                    .map(Self::project_catalog_item)
                    .collect();
                let total = body
                    .get("resultSetSize")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let next_pos = (offset.unwrap_or(0) as u64).saturating_add(items.len() as u64);

                let mut out = ProtocolOutput::new(KIND, base_url, op);
                out.items = items;
                if !out.items.is_empty() && next_pos < total {
                    out.next_cursor = Some(next_pos.to_string());
                }
                Ok(out)
            }
            "rows" => {
                let raw_id = req_str(map, "dataset_id")?;
                let dataset_id = validate_path_segment("dataset_id", &raw_id)?;
                let limit = opt_limit(map, 20, 1, 100)?;
                let offset = opt_int_range(map, "offset", 0, 100_000)?;
                let where_clause = opt_str(map, "where")?;
                let q = opt_str(map, "q")?;

                let mut req = HttpRequest::get(format!(
                    "{base_no_slash}/resource/{}.json",
                    encode_segment(&dataset_id)
                ))
                .header("Accept", "application/json")
                .query_param("$limit", limit.to_string());
                if let Some(off) = offset {
                    req = req.query_param("$offset", off.to_string());
                }
                if let Some(w) = where_clause {
                    req = req.query_param("$where", w);
                }
                if let Some(q_val) = q {
                    req = req.query_param("$q", q_val);
                }

                let resp = send_request(self.net.as_ref(), req, cx, KIND).await?;
                let body = parse_json_body(&resp, KIND)?;
                let rows = body.as_array().cloned().ok_or_else(|| {
                    FsmError::upstream(
                        KIND,
                        Some(resp.status),
                        "Socrata SODA rows response was not a JSON array",
                    )
                })?;

                let mut out = ProtocolOutput::new(KIND, base_url, op);
                out.items = rows.into_iter().take(limit).collect();
                Ok(out)
            }
            _ => Err(unknown_op(KIND, op, &["search", "rows"])),
        }
    }
}
