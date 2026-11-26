//! SRU / CQL library catalog protocol adapter (`sru`, A1 §E #2).

use crate::common::{
    child_by_local, child_text, children_by_local, collapse_ws, dc_node_to_work, decode_body,
    descendants_by_local, dialect_opt_str, node_text, normalize_date, normalize_doi, opt_int,
    opt_limit, opt_str, opt_str_aliases, params_map, parse_xml, reject_unknown_dialect_keys,
    req_str, send_checked, validate_base_url, work_to_value, year_from_date,
};
use fsm_core::{
    AuthorRef, FsmError, HttpRequest, OpDecl, Param, ParamType, ProtocolAdapter, ProtocolOutput,
    ReturnKind, ScholarlyWork, SourceDeps, SrcCx,
};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::LazyLock;

const KIND: &str = "sru";
const DIALECT_KEYS: &[&str] = &["version", "record_schema", "query_template"];

static CQL_CLAUSE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^\s*\(*\s*(?:[a-z][a-z0-9_.-]*\s*(?:==|<>|<=|>=|=|<|>)|(?:[a-z][a-z0-9_-]*\.[a-z0-9_.-]+|(?:cql|title|author|subject|isbn|issn|creator|date|tit|per|sw|num))\s+(?:all|any|exact|adj|within|prox)\s+)",
    )
    .expect("regex")
});

pub struct SruAdapter {
    deps: SourceDeps,
    ops: Vec<OpDecl>,
}

impl SruAdapter {
    pub fn new(deps: SourceDeps) -> Self {
        let ops = vec![
            OpDecl::new(
                "search",
                "Search an SRU 1.1/1.2 library catalog via CQL query.",
                ReturnKind::Works,
            )
            .with_param(
                Param::new("query", ParamType::String, "Keyword or CQL search query").required(),
            )
            .with_param(
                Param::new("limit", ParamType::Int, "Maximum records to return")
                    .with_default(json!(10))
                    .with_range(1.0, 50.0),
            )
            .with_param(
                Param::new("start", ParamType::Int, "1-based startRecord offset")
                    .with_default(json!(1))
                    .with_range(1.0, 10_000.0),
            )
            .with_param(
                Param::new(
                    "version",
                    ParamType::String,
                    "SRU protocol version (1.1 or 1.2)",
                )
                .with_enum(&["1.1", "1.2"]),
            )
            .with_param(Param::new(
                "record_schema",
                ParamType::String,
                "SRU recordSchema (e.g. dc, oai_dc, dublincore, dcndl); also accepts recordSchema",
            ))
            .with_param(Param::new(
                "recordSchema",
                ParamType::String,
                "Alias for record_schema (SRU recordSchema)",
            )),
            OpDecl::new(
                "explain",
                "Inspect an SRU server's ZeeRex explain metadata (indexes and schemas).",
                ReturnKind::Records,
            )
            .with_param(
                Param::new(
                    "version",
                    ParamType::String,
                    "SRU protocol version (1.1 or 1.2)",
                )
                .with_enum(&["1.1", "1.2"]),
            ),
        ];
        Self { deps, ops }
    }

    async fn call_search(
        &self,
        base_url: &str,
        params: &Map<String, Value>,
        cx: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError> {
        let raw_query = req_str(params, "query")?;
        let limit = opt_limit(params, "limit", 10, 1, 50)?;
        let start = opt_int(params, "start", 1, 10_000)?.unwrap_or(1) as usize;
        let dialect_version = dialect_opt_str(KIND, &cx.dialect, "version")?;
        let dialect_schema = dialect_opt_str(KIND, &cx.dialect, "record_schema")?;
        let query_template = dialect_opt_str(KIND, &cx.dialect, "query_template")?;

        let version = opt_str(params, "version")?
            .or(dialect_version)
            .unwrap_or_else(|| "1.1".to_owned());
        validate_sru_version(&version)?;
        let record_schema =
            opt_str_aliases(params, &["record_schema", "recordSchema"])?.or(dialect_schema);

        let cql = format_cql_query(&raw_query, query_template.as_deref());

        let mut req = HttpRequest::get(base_url)
            .query_param("version", &version)
            .query_param("operation", "searchRetrieve")
            .query_param("query", &cql);
        if let Some(schema) = &record_schema {
            req = req.query_param("recordSchema", schema);
        }
        req = req.query_param("maximumRecords", limit.to_string());
        if start > 1 {
            req = req.query_param("startRecord", start.to_string());
        }
        req = req.header("Accept", "application/xml, text/xml");

        let resp = send_checked(&self.deps, req, cx, KIND).await?;
        let body = decode_body(&resp);
        let doc = parse_xml(&body, KIND, resp.status)?;
        let root = doc.root_element();
        if !root
            .tag_name()
            .name()
            .eq_ignore_ascii_case("searchRetrieveResponse")
        {
            return Err(FsmError::upstream(
                KIND,
                Some(resp.status),
                format!(
                    "SRU search response root <{}> is not searchRetrieveResponse",
                    root.tag_name().name()
                ),
            ));
        }

        let mut items = Vec::new();
        if let Some(records_el) = child_by_local(root, "records") {
            for rec in children_by_local(records_el, "record") {
                if items.len() >= limit {
                    break;
                }
                if let Some(work) = parse_sru_record(rec, resp.status)? {
                    items.push(work_to_value(work));
                }
            }
        }

        if items.is_empty()
            && let Some(diag) = extract_sru_diagnostic(root)
        {
            return Err(FsmError::upstream(
                KIND,
                Some(resp.status),
                format!("SRU diagnostic: {diag}"),
            ));
        }

        let next_cursor = child_text(root, "nextRecordPosition").filter(|pos| {
            let total = child_text(root, "numberOfRecords").and_then(|s| s.parse::<usize>().ok());
            match (pos.parse::<usize>().ok(), total) {
                (Some(p), Some(t)) => p <= t,
                (Some(_), None) => true,
                _ => false,
            }
        });

        let mut out = ProtocolOutput::new(KIND, base_url, "search");
        out.items = items;
        out.next_cursor = next_cursor;
        Ok(out)
    }

    async fn call_explain(
        &self,
        base_url: &str,
        params: &Map<String, Value>,
        cx: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError> {
        let dialect_version = dialect_opt_str(KIND, &cx.dialect, "version")?;
        let version = opt_str(params, "version")?
            .or(dialect_version)
            .unwrap_or_else(|| "1.1".to_owned());
        validate_sru_version(&version)?;

        let req = HttpRequest::get(base_url)
            .query_param("version", &version)
            .query_param("operation", "explain")
            .header("Accept", "application/xml, text/xml");

        let resp = send_checked(&self.deps, req, cx, KIND).await?;
        let body = decode_body(&resp);
        let doc = parse_xml(&body, KIND, resp.status)?;
        let root = doc.root_element();

        let has_explain = descendants_by_local(root, "explain").next().is_some();
        if let Some(diag) = extract_sru_diagnostic(root)
            && !has_explain
        {
            return Err(FsmError::upstream(
                KIND,
                Some(resp.status),
                format!("SRU diagnostic: {diag}"),
            ));
        }
        if !root
            .tag_name()
            .name()
            .eq_ignore_ascii_case("explainResponse")
            && !has_explain
        {
            return Err(FsmError::upstream(
                KIND,
                Some(resp.status),
                format!(
                    "SRU explain response missing <explainResponse>/<explain>, got <{}>",
                    root.tag_name().name()
                ),
            ));
        }

        let mut rec = Map::new();
        rec.insert("url".into(), Value::String(base_url.to_owned()));
        rec.insert(
            "version".into(),
            Value::String(child_text(root, "version").unwrap_or(version)),
        );
        if let Some(db) = descendants_by_local(root, "databaseInfo").next() {
            if let Some(t) = child_text(db, "title") {
                rec.insert("title".into(), Value::String(t));
            }
            if let Some(d) = child_text(db, "description") {
                rec.insert("description".into(), Value::String(d));
            }
        }
        if let Some(srv) = descendants_by_local(root, "serverInfo").next() {
            if let Some(h) = child_text(srv, "host") {
                rec.insert("host".into(), Value::String(h));
            }
            if let Some(db) = child_text(srv, "database") {
                rec.insert("database".into(), Value::String(db));
            }
        }
        let schemas: Vec<Value> = descendants_by_local(root, "schema")
            .filter_map(|s| s.attribute("name").or_else(|| s.attribute("identifier")))
            .map(|s| Value::String(s.to_owned()))
            .collect();
        if !schemas.is_empty() {
            rec.insert("schemas".into(), Value::Array(schemas));
        }
        let indexes: Vec<Value> = descendants_by_local(root, "index")
            .filter_map(|idx| {
                descendants_by_local(idx, "name")
                    .next()
                    .map(node_text)
                    .filter(|s| !s.is_empty())
            })
            .map(Value::String)
            .collect();
        if !indexes.is_empty() {
            rec.insert("indexes".into(), Value::Array(indexes));
        }

        let mut out = ProtocolOutput::new(KIND, base_url, "explain");
        out.items = vec![Value::Object(rec)];
        Ok(out)
    }
}

fn validate_sru_version(version: &str) -> Result<(), FsmError> {
    if matches!(version, "1.1" | "1.2") {
        Ok(())
    } else {
        Err(FsmError::invalid(format!(
            "{KIND}: version must be \"1.1\" or \"1.2\", got {version:?}"
        )))
    }
}

/// CQL wrapping rule: transmit verbatim only when `query` begins with a CQL
/// index + relation clause (`index = …`, `index all …`, etc.), else substitute
/// `{query}` (escaping `"` as `\"`, and quoting when the template does not
/// already wrap `{query}` in quotes) into `query_template` if set.
pub(crate) fn format_cql_query(query: &str, query_template: Option<&str>) -> String {
    if has_cql_operator(query) {
        return query.to_owned();
    }
    match query_template {
        Some(tpl) => {
            let escaped = query.replace('\\', "\\\\").replace('"', "\\\"");
            if tpl.contains("\"{query}\"") {
                tpl.replace("{query}", &escaped)
            } else {
                tpl.replace("{query}", &format!("\"{escaped}\""))
            }
        }
        None => query.to_owned(),
    }
}

fn has_cql_operator(query: &str) -> bool {
    CQL_CLAUSE_RE.is_match(query)
}

fn extract_sru_diagnostic(root: roxmltree::Node<'_, '_>) -> Option<String> {
    let diag = descendants_by_local(root, "diagnostic").next()?;
    let msg = child_text(diag, "message");
    let details = child_text(diag, "details");
    let uri = child_text(diag, "uri");
    match (msg, details, uri) {
        (Some(m), Some(d), _) => Some(format!("{m} ({d})")),
        (Some(m), None, _) => Some(m),
        (None, Some(d), _) => Some(d),
        (None, None, Some(u)) => Some(u),
        _ => Some("unknown diagnostic".to_owned()),
    }
}

fn parse_sru_record(
    rec: roxmltree::Node<'_, '_>,
    status: u16,
) -> Result<Option<ScholarlyWork>, FsmError> {
    let packing = child_text(rec, "recordPacking").unwrap_or_else(|| "xml".to_owned());
    let Some(data) = child_by_local(rec, "recordData") else {
        return Ok(None);
    };

    if packing.eq_ignore_ascii_case("string") {
        // String-packed SRU records (e.g. NDL Japan dcndl) embed XML-escaped markup as text.
        // Propagate parse_xml errors (Ruling R3: never swallow entity/size rejections).
        let inner_xml = data.text().unwrap_or("").trim();
        if inner_xml.is_empty() {
            return Ok(None);
        }
        let inner_doc = parse_xml(inner_xml, KIND, status)?;
        return Ok(parse_record_payload(inner_doc.root_element()));
    }

    let payload = data.children().find(|n| n.is_element()).unwrap_or(data);
    Ok(parse_record_payload(payload))
}

fn parse_record_payload(node: roxmltree::Node<'_, '_>) -> Option<ScholarlyWork> {
    // MARCXML fallback when <datafield tag="245"> is present.
    if descendants_by_local(node, "datafield").next().is_some()
        && let Some(work) = parse_marcxml(node)
    {
        return Some(work);
    }
    // Dublin Core / oai_dc / srw_dc / dcndl RDF.
    dc_node_to_work(node, None, "book")
}

fn parse_marcxml(node: roxmltree::Node<'_, '_>) -> Option<ScholarlyWork> {
    let mut title_parts = Vec::new();
    let mut authors = Vec::new();
    let mut venue: Option<String> = None;
    let mut published_date: Option<String> = None;
    let mut doi: Option<String> = None;
    let mut url: Option<String> = None;

    for df in descendants_by_local(node, "datafield") {
        let tag = df.attribute("tag").unwrap_or("");
        match tag {
            "245" => {
                for sf in children_by_local(df, "subfield") {
                    if matches!(sf.attribute("code"), Some("a" | "b")) {
                        let t = node_text(sf);
                        if !t.is_empty() {
                            title_parts.push(t);
                        }
                    }
                }
            }
            "100" | "700" => {
                for sf in children_by_local(df, "subfield") {
                    if sf.attribute("code") == Some("a") {
                        let name = node_text(sf).trim_end_matches([',', '.']).to_owned();
                        if !name.is_empty() {
                            authors.push(AuthorRef::new(name));
                        }
                    }
                }
            }
            "260" | "264" => {
                for sf in children_by_local(df, "subfield") {
                    match sf.attribute("code") {
                        Some("b") if venue.is_none() => {
                            let v = node_text(sf).trim_end_matches([',', ';', ':']).to_owned();
                            if !v.is_empty() {
                                venue = Some(v);
                            }
                        }
                        Some("c") if published_date.is_none() => {
                            let raw = node_text(sf);
                            let digits: String = raw
                                .chars()
                                .skip_while(|c| !c.is_ascii_digit())
                                .take_while(|c| c.is_ascii_digit() || *c == '-')
                                .collect();
                            published_date = normalize_date(&digits);
                        }
                        _ => {}
                    }
                }
            }
            "024" => {
                for sf in children_by_local(df, "subfield") {
                    if sf.attribute("code") == Some("a") && doi.is_none() {
                        doi = normalize_doi(&node_text(sf));
                    }
                }
            }
            "856" => {
                for sf in children_by_local(df, "subfield") {
                    if sf.attribute("code") == Some("u") && url.is_none() {
                        let u = node_text(sf);
                        if u.starts_with("http://") || u.starts_with("https://") {
                            url = Some(u);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let title = collapse_ws(&title_parts.join(" "))
        .trim_end_matches(['/', ':'])
        .trim()
        .to_owned();
    if title.is_empty() {
        return None;
    }
    let final_url = url
        .or_else(|| doi.as_ref().map(|d| format!("https://doi.org/{d}")))
        .unwrap_or_default();
    let mut work = ScholarlyWork::new(title, final_url);
    work.authors = authors;
    work.venue = venue;
    work.year = published_date.as_deref().and_then(year_from_date);
    work.published_date = published_date;
    work.work_type = "book".to_owned();
    work.ids.doi = doi;
    Some(work)
}

#[async_trait::async_trait]
impl ProtocolAdapter for SruAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn ops(&self) -> &[OpDecl] {
        &self.ops
    }

    fn validate_dialect(&self, dialect: &BTreeMap<String, Value>) -> Result<(), FsmError> {
        reject_unknown_dialect_keys(KIND, dialect, DIALECT_KEYS)?;
        if let Some(v) = dialect_opt_str(KIND, dialect, "version")? {
            validate_sru_version(&v)?;
        }
        let _ = dialect_opt_str(KIND, dialect, "record_schema")?;
        if let Some(tpl) = dialect_opt_str(KIND, dialect, "query_template")?
            && !tpl.contains("{query}")
        {
            return Err(FsmError::invalid(format!(
                "{KIND}: dialect.query_template must contain \"{{query}}\""
            )));
        }
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
            "explain" => self.call_explain(base_url, map, cx).await,
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
    fn cql_wrapping_preserves_operators_and_escapes_plain_queries() {
        let tpl = Some("bib.title all \"{query}\"");
        assert_eq!(format_cql_query("neural", tpl), "bib.title all \"neural\"");
        assert_eq!(
            format_cql_query("deep \"neural\"", tpl),
            "bib.title all \"deep \\\"neural\\\"\""
        );
        for verbatim in [
            "dc.title=neural",
            "bib.title all \"neural\"",
            "dc.title any \"a b\"",
            "dc.title exact \"foo\"",
            "dc.title adj \"foo bar\"",
            "dc.title prox \"foo bar\"",
        ] {
            assert_eq!(format_cql_query(verbatim, tpl), verbatim);
        }
        assert_eq!(format_cql_query("plain", None), "plain");
        // Natural-language phrases containing "all" must still be wrapped by query_template.
        assert_eq!(
            format_cql_query("Attention is all you need", tpl),
            "bib.title all \"Attention is all you need\""
        );
        // Unquoted templates auto-quote the substituted value for strict SRU servers.
        assert_eq!(
            format_cql_query("machine learning", Some("dc.title={query}")),
            "dc.title=\"machine learning\""
        );
        assert_eq!(
            format_cql_query("deep \"learning\"", Some("dc.title={query}")),
            "dc.title=\"deep \\\"learning\\\"\""
        );
    }
}
