//! SPARQL 1.1 Query Protocol adapter (`sparql`, A1 §E #4).

use crate::common::{
    dialect_opt_str, dialect_opt_str_map, opt_limit, params_map, parse_json,
    reject_unknown_dialect_keys, req_str, send_checked, validate_base_url,
};
use fsm_core::{
    FsmError, HttpRequest, OpDecl, Param, ParamType, ProtocolAdapter, ProtocolOutput, ReturnKind,
    SourceDeps, SrcCx,
};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::LazyLock;

const KIND: &str = "sparql";
const DIALECT_KEYS: &[&str] = &["method", "accept", "default_graph_uri", "prefixes"];
const DEFAULT_ACCEPT: &str = "application/sparql-results+json";

static HAS_LIMIT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bLIMIT\s+\d+\b").expect("regex"));
static IS_SELECT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bSELECT\b").expect("regex"));

pub struct SparqlAdapter {
    deps: SourceDeps,
    ops: Vec<OpDecl>,
}

impl SparqlAdapter {
    pub fn new(deps: SourceDeps) -> Self {
        let ops = vec![
            OpDecl::new(
                "query",
                "Execute a SPARQL 1.1 SELECT or ASK query and return flattened bindings.",
                ReturnKind::Records,
            )
            .with_param(
                Param::new("query", ParamType::String, "SPARQL 1.1 SELECT or ASK query").required(),
            )
            .with_param(
                Param::new(
                    "limit",
                    ParamType::Int,
                    "Default row limit (applied only when query lacks LIMIT)",
                )
                .with_default(json!(50))
                .with_range(1.0, 200.0),
            ),
        ];
        Self { deps, ops }
    }

    async fn call_query(
        &self,
        base_url: &str,
        params: &Map<String, Value>,
        cx: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError> {
        let raw_query = req_str(params, "query")?;
        let limit = opt_limit(params, "limit", 50, 1, 200)?;

        let method =
            dialect_opt_str(KIND, &cx.dialect, "method")?.unwrap_or_else(|| "GET".to_owned());
        let accept = dialect_opt_str(KIND, &cx.dialect, "accept")?
            .unwrap_or_else(|| DEFAULT_ACCEPT.to_owned());
        let default_graph_uri = dialect_opt_str(KIND, &cx.dialect, "default_graph_uri")?;
        let prefixes = dialect_opt_str_map(KIND, &cx.dialect, "prefixes")?;

        let (final_query, appended_limit) =
            prepare_sparql_query(&raw_query, limit, prefixes.as_ref());

        let mut req = if method.eq_ignore_ascii_case("POST") {
            HttpRequest::post(base_url, final_query.into_bytes())
                .header("Content-Type", "application/sparql-query")
                .header("Accept", &accept)
        } else {
            HttpRequest::get(base_url)
                .query_param("query", &final_query)
                .header("Accept", &accept)
        };
        if let Some(graph_uri) = default_graph_uri {
            req = req.query_param("default-graph-uri", graph_uri);
        }

        let resp = send_checked(&self.deps, req, cx, KIND).await?;
        let json_val = parse_json(&resp, KIND)?;

        // Truncate rows only when the adapter itself applied `limit` (user-supplied LIMIT stays intact).
        let row_cap = appended_limit.then_some(limit);
        let items = parse_sparql_results(&json_val, row_cap, resp.status)?;
        let mut out = ProtocolOutput::new(KIND, base_url, "query");
        out.items = items;
        Ok(out)
    }
}

/// Build a scan skeleton of a SPARQL query for `PREFIX`/`SELECT`/`LIMIT` detection:
/// strips `# ...` line comments and blanks out string literal contents so keywords
/// like `"speed LIMIT 10"` inside strings or comments cannot suppress auto-`LIMIT`
/// (while leaving `<IRI>` intact and transmitting `raw_query` verbatim on the wire).
fn sparql_scan_skeleton(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'<' => {
                let start = i;
                i += 1;
                while i < bytes.len() && !matches!(bytes[i], b'>' | b' ' | b'\t' | b'\r' | b'\n') {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'>' {
                    i += 1;
                }
                out.push_str(&input[start..i]);
            }
            b'"' | b'\'' => {
                let q = bytes[i];
                let triple = i + 2 < bytes.len() && bytes[i + 1] == q && bytes[i + 2] == q;
                i += if triple { 3 } else { 1 };
                while i < bytes.len() {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                        continue;
                    }
                    if triple {
                        if i + 2 < bytes.len()
                            && bytes[i] == q
                            && bytes[i + 1] == q
                            && bytes[i + 2] == q
                        {
                            i += 3;
                            break;
                        }
                    } else if bytes[i] == q {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                out.push(q as char);
                out.push(q as char);
            }
            b'#' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            _ => {
                let ch = input[i..].chars().next().expect("valid utf-8");
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// Prepend missing `PREFIX` declarations and append `\nLIMIT {limit}` only when
/// the query is a `SELECT` without an existing `LIMIT` clause (A1 §E #4).
/// Returns `(prepared_query, appended_limit)`.
fn prepare_sparql_query(
    raw_query: &str,
    limit: usize,
    prefixes: Option<&BTreeMap<String, String>>,
) -> (String, bool) {
    let mut query = String::new();
    let skeleton = sparql_scan_skeleton(raw_query);
    if let Some(map) = prefixes {
        let upper = skeleton.to_ascii_uppercase();
        for (prefix, iri) in map {
            let needle = format!("PREFIX {}:", prefix.to_ascii_uppercase());
            if !upper.contains(&needle) {
                query.push_str(&format!("PREFIX {prefix}: <{iri}>\n"));
            }
        }
    }
    query.push_str(raw_query);
    let appended_limit = IS_SELECT_RE.is_match(&skeleton) && !HAS_LIMIT_RE.is_match(&skeleton);
    if appended_limit {
        // Leading newline prevents a trailing `# comment` from swallowing `LIMIT`.
        query.push_str(&format!("\nLIMIT {limit}"));
    }
    (query, appended_limit)
}

fn parse_sparql_results(
    val: &Value,
    row_cap: Option<usize>,
    status: u16,
) -> Result<Vec<Value>, FsmError> {
    if let Some(b) = val.get("boolean").and_then(Value::as_bool) {
        return Ok(vec![json!({ "boolean": b })]);
    }
    let Some(bindings) = val
        .get("results")
        .and_then(|r| r.get("bindings"))
        .and_then(Value::as_array)
    else {
        return Err(FsmError::upstream(
            KIND,
            Some(status),
            "SPARQL JSON response missing results.bindings or boolean",
        ));
    };

    let take_n = row_cap.map_or(bindings.len(), |c| bindings.len().min(c));
    let mut rows = Vec::with_capacity(take_n);
    for binding in bindings.iter().take(take_n) {
        let Some(obj) = binding.as_object() else {
            continue;
        };
        let mut row = Map::new();
        for (var, cell) in obj {
            if let Some(v) = cell.get("value").and_then(Value::as_str) {
                row.insert(var.clone(), Value::String(v.to_owned()));
            }
        }
        rows.push(Value::Object(row));
    }
    Ok(rows)
}

#[async_trait::async_trait]
impl ProtocolAdapter for SparqlAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn ops(&self) -> &[OpDecl] {
        &self.ops
    }

    fn validate_dialect(&self, dialect: &BTreeMap<String, Value>) -> Result<(), FsmError> {
        reject_unknown_dialect_keys(KIND, dialect, DIALECT_KEYS)?;
        if let Some(m) = dialect_opt_str(KIND, dialect, "method")?
            && !matches!(m.as_str(), "GET" | "POST")
        {
            return Err(FsmError::invalid(format!(
                "{KIND}: dialect.method must be \"GET\" or \"POST\", got {m:?}"
            )));
        }
        let _ = dialect_opt_str(KIND, dialect, "accept")?;
        let _ = dialect_opt_str(KIND, dialect, "default_graph_uri")?;
        let _ = dialect_opt_str_map(KIND, dialect, "prefixes")?;
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
            "query" => self.call_query(base_url, map, cx).await,
            other => Err(FsmError::invalid(format!(
                "unknown op {other:?} for protocol {KIND}"
            ))),
        }
    }
}
