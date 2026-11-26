//! OAI-PMH 2.0 protocol adapter (`oai_pmh`, A1 §E #1).

use crate::common::{
    child_by_local, child_text, children_by_local, dc_node_to_work, descendants_by_local,
    dialect_opt_str, node_text, opt_limit, opt_str, params_map, parse_xml,
    reject_unknown_dialect_keys, req_str, send_checked, validate_base_url, work_to_value,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use fsm_core::{
    FsmError, HttpRequest, OpDecl, Param, ParamType, ProtocolAdapter, ProtocolOutput, ReturnKind,
    SourceDeps, SrcCx,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

const KIND: &str = "oai_pmh";
const DIALECT_KEYS: &[&str] = &["metadata_prefix", "default_set"];
const OAI_SKIP_PREFIX: &str = "oaiskip:";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OaiInitialArgs {
    p: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    s: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    f: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    u: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OaiSkipCursor {
    #[serde(skip_serializing_if = "Option::is_none")]
    t: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    a: Option<OaiInitialArgs>,
    skip: usize,
}

fn encode_skip_cursor(c: &OaiSkipCursor) -> String {
    let raw = serde_json::to_vec(c).expect("OaiSkipCursor serializes");
    format!("{OAI_SKIP_PREFIX}{}", URL_SAFE_NO_PAD.encode(raw))
}

fn decode_skip_cursor(cursor: &str) -> Result<Option<OaiSkipCursor>, FsmError> {
    let Some(b64) = cursor.strip_prefix(OAI_SKIP_PREFIX) else {
        return Ok(None);
    };
    let bytes = URL_SAFE_NO_PAD
        .decode(b64)
        .map_err(|e| FsmError::invalid(format!("{KIND}: malformed oaiskip cursor ({e})")))?;
    let parsed: OaiSkipCursor = serde_json::from_slice(&bytes)
        .map_err(|e| FsmError::invalid(format!("{KIND}: malformed oaiskip cursor JSON ({e})")))?;
    if parsed.t.is_none() && parsed.a.is_none() {
        return Err(FsmError::invalid(format!(
            "{KIND}: oaiskip cursor missing both token and initial args"
        )));
    }
    Ok(Some(parsed))
}

pub struct OaiPmhAdapter {
    deps: SourceDeps,
    ops: Vec<OpDecl>,
}

impl OaiPmhAdapter {
    pub fn new(deps: SourceDeps) -> Self {
        let ops = vec![
            OpDecl::new(
                "identify",
                "Identify an OAI-PMH 2.0 repository and its harvesting capabilities.",
                ReturnKind::Records,
            ),
            OpDecl::new(
                "list_sets",
                "List OAI-PMH set hierarchy (supports resumptionToken cursor).",
                ReturnKind::Records,
            )
            .with_param(Param::new(
                "cursor",
                ParamType::String,
                "OAI-PMH resumptionToken for the next page of sets",
            )),
            OpDecl::new(
                "list_records",
                "Harvest scholarly records via OAI-PMH ListRecords.",
                ReturnKind::Works,
            )
            .with_param(Param::new(
                "metadata_prefix",
                ParamType::String,
                "OAI-PMH metadataPrefix (defaults to dialect.metadata_prefix, else oai_dc)",
            ))
            .with_param(Param::new(
                "set",
                ParamType::String,
                "Optional OAI-PMH setSpec filter",
            ))
            .with_param(Param::new(
                "from",
                ParamType::String,
                "Optional lower datestamp bound (YYYY-MM-DD or ISO-8601)",
            ))
            .with_param(Param::new(
                "until",
                ParamType::String,
                "Optional upper datestamp bound (YYYY-MM-DD or ISO-8601)",
            ))
            .with_param(Param::new(
                "cursor",
                ParamType::String,
                "Opaque OAI-PMH cursor (resumptionToken or composite slice cursor)",
            ))
            .with_param(
                Param::new("limit", ParamType::Int, "Maximum works to return")
                    .with_default(json!(20))
                    .with_range(1.0, 100.0),
            ),
            OpDecl::new(
                "get_record",
                "Fetch a single scholarly record by OAI-PMH identifier.",
                ReturnKind::Works,
            )
            .with_param(
                Param::new("identifier", ParamType::String, "OAI-PMH item identifier").required(),
            )
            .with_param(Param::new(
                "metadata_prefix",
                ParamType::String,
                "OAI-PMH metadataPrefix (defaults to dialect.metadata_prefix, else oai_dc)",
            )),
        ];
        Self { deps, ops }
    }

    async fn call_identify(&self, base_url: &str, cx: &SrcCx) -> Result<ProtocolOutput, FsmError> {
        let req = HttpRequest::get(base_url)
            .query_param("verb", "Identify")
            .header("Accept", "application/xml, text/xml");
        let resp = send_checked(&self.deps, req, cx, KIND).await?;
        let body = crate::common::decode_body(&resp);
        let doc = parse_xml(&body, KIND, resp.status)?;
        let root = doc.root_element();
        check_oai_error(root, resp.status)?;

        let ident = child_by_local(root, "Identify").ok_or_else(|| {
            FsmError::upstream(
                KIND,
                Some(resp.status),
                "OAI-PMH response missing <Identify>",
            )
        })?;

        let mut rec = Map::new();
        let repo_name = child_text(ident, "repositoryName").unwrap_or_default();
        let reported_base = child_text(ident, "baseURL").unwrap_or_else(|| base_url.to_owned());
        rec.insert("repositoryName".into(), Value::String(repo_name.clone()));
        rec.insert("title".into(), Value::String(repo_name));
        rec.insert("baseUrl".into(), Value::String(reported_base.clone()));
        rec.insert("url".into(), Value::String(reported_base));
        if let Some(v) = child_text(ident, "protocolVersion") {
            rec.insert("protocolVersion".into(), Value::String(v));
        }
        let emails: Vec<Value> = children_by_local(ident, "adminEmail")
            .map(node_text)
            .filter(|s| !s.is_empty())
            .map(Value::String)
            .collect();
        if !emails.is_empty() {
            rec.insert("adminEmail".into(), Value::Array(emails));
        }
        if let Some(v) = child_text(ident, "earliestDatestamp") {
            rec.insert("earliestDatestamp".into(), Value::String(v));
        }
        if let Some(v) = child_text(ident, "deletedRecord") {
            rec.insert("deletedRecord".into(), Value::String(v));
        }
        if let Some(v) = child_text(ident, "granularity") {
            rec.insert("granularity".into(), Value::String(v));
        }
        if let Some(oai_id) = descendants_by_local(ident, "oai-identifier").next() {
            if let Some(v) = child_text(oai_id, "repositoryIdentifier") {
                rec.insert("repositoryIdentifier".into(), Value::String(v));
            }
            if let Some(v) = child_text(oai_id, "sampleIdentifier") {
                rec.insert("sampleIdentifier".into(), Value::String(v));
            }
        }

        let mut out = ProtocolOutput::new(KIND, base_url, "identify");
        out.items = vec![Value::Object(rec)];
        Ok(out)
    }

    async fn call_list_sets(
        &self,
        base_url: &str,
        params: &Map<String, Value>,
        cx: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError> {
        let cursor = opt_str(params, "cursor")?;
        let mut req = HttpRequest::get(base_url)
            .query_param("verb", "ListSets")
            .header("Accept", "application/xml, text/xml");
        if let Some(token) = cursor {
            req = req.query_param("resumptionToken", token);
        }
        let resp = send_checked(&self.deps, req, cx, KIND).await?;
        let body = crate::common::decode_body(&resp);
        let doc = parse_xml(&body, KIND, resp.status)?;
        let root = doc.root_element();
        if is_oai_error_code(root, "noSetHierarchy") {
            return Ok(ProtocolOutput::new(KIND, base_url, "list_sets"));
        }
        check_oai_error(root, resp.status)?;

        let list_sets = child_by_local(root, "ListSets").ok_or_else(|| {
            FsmError::upstream(
                KIND,
                Some(resp.status),
                "OAI-PMH response missing <ListSets>",
            )
        })?;

        let mut items = Vec::new();
        for set_node in children_by_local(list_sets, "set") {
            let spec = child_text(set_node, "setSpec").unwrap_or_default();
            let name = child_text(set_node, "setName").unwrap_or_else(|| spec.clone());
            if spec.is_empty() && name.is_empty() {
                continue;
            }
            let mut rec = Map::new();
            rec.insert("setSpec".into(), Value::String(spec));
            rec.insert("setName".into(), Value::String(name));
            if let Some(desc) = child_by_local(set_node, "setDescription") {
                let text = node_text(desc);
                if !text.is_empty() {
                    rec.insert("description".into(), Value::String(text));
                }
            }
            items.push(Value::Object(rec));
        }
        let next_cursor = child_text(list_sets, "resumptionToken");

        let mut out = ProtocolOutput::new(KIND, base_url, "list_sets");
        out.items = items;
        out.next_cursor = next_cursor;
        Ok(out)
    }

    async fn call_list_records(
        &self,
        base_url: &str,
        params: &Map<String, Value>,
        cx: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError> {
        let dialect_prefix = dialect_opt_str(KIND, &cx.dialect, "metadata_prefix")?;
        let dialect_set = dialect_opt_str(KIND, &cx.dialect, "default_set")?;

        let prefix = opt_str(params, "metadata_prefix")?
            .or(dialect_prefix)
            .unwrap_or_else(|| "oai_dc".to_owned());
        let set = opt_str(params, "set")?.or(dialect_set);
        let from = opt_str(params, "from")?;
        let until = opt_str(params, "until")?;
        let cursor = opt_str(params, "cursor")?;
        let limit = opt_limit(params, "limit", 20, 1, 100)?;

        let (req_token, req_args, skip) = match cursor {
            Some(ref raw) => match decode_skip_cursor(raw)? {
                Some(sc) => (sc.t, sc.a, sc.skip),
                None => (Some(raw.clone()), None, 0),
            },
            None => (
                None,
                Some(OaiInitialArgs {
                    p: prefix,
                    s: set,
                    f: from,
                    u: until,
                }),
                0,
            ),
        };

        let mut req = HttpRequest::get(base_url)
            .query_param("verb", "ListRecords")
            .header("Accept", "application/xml, text/xml");
        if let Some(ref token) = req_token {
            // OAI-PMH 2.0: resumptionToken is an exclusive argument.
            req = req.query_param("resumptionToken", token);
        } else if let Some(ref args) = req_args {
            req = req.query_param("metadataPrefix", &args.p);
            if let Some(ref s) = args.s {
                req = req.query_param("set", s);
            }
            if let Some(ref f) = args.f {
                req = req.query_param("from", f);
            }
            if let Some(ref u) = args.u {
                req = req.query_param("until", u);
            }
        }

        let resp = send_checked(&self.deps, req, cx, KIND).await?;
        let body = crate::common::decode_body(&resp);
        let doc = parse_xml(&body, KIND, resp.status)?;
        let root = doc.root_element();
        if is_oai_error_code(root, "noRecordsMatch") {
            return Ok(ProtocolOutput::new(KIND, base_url, "list_records"));
        }
        check_oai_error(root, resp.status)?;

        let list = child_by_local(root, "ListRecords").ok_or_else(|| {
            FsmError::upstream(
                KIND,
                Some(resp.status),
                "OAI-PMH response missing <ListRecords>",
            )
        })?;

        let mut all_works = Vec::new();
        for rec in children_by_local(list, "record") {
            if let Some(work) = parse_oai_record(rec) {
                all_works.push(work_to_value(work));
            }
        }
        let server_token = child_text(list, "resumptionToken");

        let items: Vec<Value> = all_works.iter().skip(skip).take(limit).cloned().collect();
        let consumed_to = skip.saturating_add(items.len());
        let next_cursor = if consumed_to < all_works.len() {
            Some(encode_skip_cursor(&OaiSkipCursor {
                t: req_token,
                a: req_args,
                skip: consumed_to,
            }))
        } else {
            server_token
        };

        let mut out = ProtocolOutput::new(KIND, base_url, "list_records");
        out.items = items;
        out.next_cursor = next_cursor;
        Ok(out)
    }

    async fn call_get_record(
        &self,
        base_url: &str,
        params: &Map<String, Value>,
        cx: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError> {
        let identifier = req_str(params, "identifier")?;
        let dialect_prefix = dialect_opt_str(KIND, &cx.dialect, "metadata_prefix")?;
        let prefix = opt_str(params, "metadata_prefix")?
            .or(dialect_prefix)
            .unwrap_or_else(|| "oai_dc".to_owned());

        let req = HttpRequest::get(base_url)
            .query_param("verb", "GetRecord")
            .query_param("identifier", identifier)
            .query_param("metadataPrefix", prefix)
            .header("Accept", "application/xml, text/xml");

        let resp = send_checked(&self.deps, req, cx, KIND).await?;
        let body = crate::common::decode_body(&resp);
        let doc = parse_xml(&body, KIND, resp.status)?;
        let root = doc.root_element();
        check_oai_error(root, resp.status)?;

        let get_rec = child_by_local(root, "GetRecord").ok_or_else(|| {
            FsmError::upstream(
                KIND,
                Some(resp.status),
                "OAI-PMH response missing <GetRecord>",
            )
        })?;

        let mut items = Vec::new();
        if let Some(rec) = child_by_local(get_rec, "record")
            && let Some(work) = parse_oai_record(rec)
        {
            items.push(work_to_value(work));
        }

        let mut out = ProtocolOutput::new(KIND, base_url, "get_record");
        out.items = items;
        Ok(out)
    }
}

fn is_oai_error_code(root: roxmltree::Node<'_, '_>, want: &str) -> bool {
    child_by_local(root, "error")
        .and_then(|e| e.attribute("code"))
        .is_some_and(|c| c.eq_ignore_ascii_case(want))
}

fn check_oai_error(root: roxmltree::Node<'_, '_>, status: u16) -> Result<(), FsmError> {
    if let Some(err) = child_by_local(root, "error") {
        let code = err.attribute("code").unwrap_or("unknown");
        let msg = node_text(err);
        return Err(FsmError::upstream(
            KIND,
            Some(status),
            if msg.is_empty() {
                format!("OAI-PMH error: {code}")
            } else {
                format!("OAI-PMH error ({code}): {msg}")
            },
        ));
    }
    Ok(())
}

fn parse_oai_record(rec: roxmltree::Node<'_, '_>) -> Option<fsm_core::ScholarlyWork> {
    let header = child_by_local(rec, "header");
    if let Some(h) = header
        && h.attribute("status")
            .is_some_and(|s| s.eq_ignore_ascii_case("deleted"))
    {
        return None;
    }
    let header_datestamp = header.and_then(|h| child_text(h, "datestamp"));
    let header_id = header.and_then(|h| child_text(h, "identifier"));
    let meta = child_by_local(rec, "metadata")?;
    let payload = meta.children().find(|n| n.is_element()).unwrap_or(meta);

    let fallback_url = header_id
        .as_deref()
        .filter(|id| id.starts_with("http://") || id.starts_with("https://"));
    let mut work = dc_node_to_work(payload, fallback_url, "article")?;
    if work.published_date.is_none()
        && let Some(ds) = header_datestamp.as_deref()
        && let Some(norm) = crate::common::normalize_date(ds)
    {
        work.year = crate::common::year_from_date(&norm);
        work.published_date = Some(norm);
    }
    Some(work)
}

#[async_trait::async_trait]
impl ProtocolAdapter for OaiPmhAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn ops(&self) -> &[OpDecl] {
        &self.ops
    }

    fn validate_dialect(&self, dialect: &BTreeMap<String, Value>) -> Result<(), FsmError> {
        reject_unknown_dialect_keys(KIND, dialect, DIALECT_KEYS)?;
        let _ = dialect_opt_str(KIND, dialect, "metadata_prefix")?;
        let _ = dialect_opt_str(KIND, dialect, "default_set")?;
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
            "identify" => self.call_identify(base_url, cx).await,
            "list_sets" => self.call_list_sets(base_url, map, cx).await,
            "list_records" => self.call_list_records(base_url, map, cx).await,
            "get_record" => self.call_get_record(base_url, map, cx).await,
            other => Err(FsmError::invalid(format!(
                "unknown op {other:?} for protocol {KIND}"
            ))),
        }
    }
}
