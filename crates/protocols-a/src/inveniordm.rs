//! InvenioRDM REST protocol adapter (`inveniordm`, A1 §E #6).

use crate::common::{
    clip, collapse_ws, dialect_opt_str, encode_path_segment, normalize_date, normalize_doi,
    normalize_work_type, opt_int, opt_limit, opt_str, params_map, parse_json,
    reject_unknown_dialect_keys, req_str, send_checked, strip_tags, trim_trailing_slash,
    validate_base_url, work_to_value, year_from_date,
};
use fsm_core::{
    AuthorRef, FsmError, HttpRequest, OaLocation, OpDecl, Param, ParamType, ProtocolAdapter,
    ProtocolOutput, ReturnKind, ScholarlyWork, SourceDeps, SrcCx,
};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

const KIND: &str = "inveniordm";
const DIALECT_KEYS: &[&str] = &["api_prefix", "community"];
const DEFAULT_API_PREFIX: &str = "/api";

pub struct InvenioRdmAdapter {
    deps: SourceDeps,
    ops: Vec<OpDecl>,
}

impl InvenioRdmAdapter {
    pub fn new(deps: SourceDeps) -> Self {
        let ops = vec![
            OpDecl::new(
                "search",
                "Search an InvenioRDM repository (records or community records).",
                ReturnKind::Works,
            )
            .with_param(Param::new("query", ParamType::String, "Search query").required())
            .with_param(
                Param::new("limit", ParamType::Int, "Maximum records per page")
                    .with_default(json!(10))
                    .with_range(1.0, 50.0),
            )
            .with_param(
                Param::new("page", ParamType::Int, "1-based page number").with_range(1.0, 10_000.0),
            )
            .with_param(Param::new(
                "sort",
                ParamType::String,
                "Optional InvenioRDM sort key (e.g. bestmatch, newest, mostviewed)",
            )),
            OpDecl::new(
                "record",
                "Fetch a single InvenioRDM record by ID.",
                ReturnKind::Works,
            )
            .with_param(Param::new("id", ParamType::String, "InvenioRDM record ID").required()),
        ];
        Self { deps, ops }
    }

    async fn call_search(
        &self,
        base_url: &str,
        params: &Map<String, Value>,
        cx: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError> {
        let query = req_str(params, "query")?;
        let limit = opt_limit(params, "limit", 10, 1, 50)?;
        let page = opt_int(params, "page", 1, 10_000)?;
        let sort = opt_str(params, "sort")?;

        let api_prefix = effective_api_prefix(&cx.dialect)?;
        let community = dialect_opt_str(KIND, &cx.dialect, "community")?;

        let url = if let Some(slug) = community {
            let encoded = encode_path_segment(&slug, "dialect.community")?;
            format!(
                "{}{}/communities/{encoded}/records",
                trim_trailing_slash(base_url),
                api_prefix
            )
        } else {
            format!("{}{}/records", trim_trailing_slash(base_url), api_prefix)
        };

        let mut req = HttpRequest::get(url)
            .query_param("q", &query)
            .query_param("size", limit.to_string())
            .header("Accept", "application/json");
        if let Some(p) = page {
            req = req.query_param("page", p.to_string());
        }
        if let Some(s) = sort {
            req = req.query_param("sort", s);
        }

        let resp = send_checked(&self.deps, req, cx, KIND).await?;
        let val = parse_json(&resp, KIND)?;

        let hits = val
            .pointer("/hits/hits")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                FsmError::upstream(
                    KIND,
                    Some(resp.status),
                    "InvenioRDM search response missing hits.hits array",
                )
            })?;

        let mut items = Vec::new();
        for hit in hits {
            if items.len() >= limit {
                break;
            }
            if let Some(work) = parse_invenio_record(hit) {
                items.push(work_to_value(work));
            }
        }

        let current_page = page.unwrap_or(1) as u64;
        let has_next_link = val
            .pointer("/links/next")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
        let total = val.pointer("/hits/total").and_then(|v| {
            v.as_u64()
                .or_else(|| v.get("value").and_then(Value::as_u64))
        });
        let next_cursor = if has_next_link
            || total
                .is_some_and(|t| current_page.saturating_mul(limit as u64) < t && !items.is_empty())
        {
            current_page.checked_add(1).map(|next| next.to_string())
        } else {
            None
        };

        let mut out = ProtocolOutput::new(KIND, base_url, "search");
        out.items = items;
        out.next_cursor = next_cursor;
        Ok(out)
    }

    async fn call_record(
        &self,
        base_url: &str,
        params: &Map<String, Value>,
        cx: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError> {
        let id = req_str(params, "id")?;
        let encoded_id = encode_path_segment(&id, "id")?;
        let api_prefix = effective_api_prefix(&cx.dialect)?;

        let url = format!(
            "{}{}/records/{encoded_id}",
            trim_trailing_slash(base_url),
            api_prefix
        );
        let req = HttpRequest::get(url).header("Accept", "application/json");
        let resp = send_checked(&self.deps, req, cx, KIND).await?;
        let val = parse_json(&resp, KIND)?;
        if val.get("metadata").and_then(Value::as_object).is_none() {
            return Err(FsmError::upstream(
                KIND,
                Some(resp.status),
                "InvenioRDM record response missing metadata object",
            ));
        }

        let mut items = Vec::new();
        if let Some(work) = parse_invenio_record(&val) {
            items.push(work_to_value(work));
        }

        let mut out = ProtocolOutput::new(KIND, base_url, "record");
        out.items = items;
        Ok(out)
    }
}

fn effective_api_prefix(dialect: &BTreeMap<String, Value>) -> Result<String, FsmError> {
    let raw = dialect_opt_str(KIND, dialect, "api_prefix")?
        .unwrap_or_else(|| DEFAULT_API_PREFIX.to_owned());
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    if trimmed.starts_with('/') {
        Ok(trimmed.to_owned())
    } else {
        Ok(format!("/{trimmed}"))
    }
}

/// Parses both native InvenioRDM v12+ records (`pids.doi.identifier`,
/// `metadata.creators[].person_or_org.name`) and Zenodo-v1-compat records
/// (such as TÜBİTAK Aperta: top-level `doi`, `metadata.creators[].name`).
pub(crate) fn parse_invenio_record(rec: &Value) -> Option<ScholarlyWork> {
    let meta = rec.get("metadata").and_then(Value::as_object)?;
    let title = meta
        .get("title")
        .and_then(Value::as_str)
        .map(collapse_ws)
        .filter(|s| !s.is_empty())?;

    // Creators: handle native InvenioRDM (person_or_org) and Zenodo-v1 (flat name).
    let mut authors = Vec::new();
    if let Some(creators) = meta.get("creators").and_then(Value::as_array) {
        for c in creators {
            if let Some(author) = parse_creator(c) {
                authors.push(author);
            }
        }
    }

    // DOI: pids.doi.identifier -> top-level doi -> metadata.doi -> links.doi
    let doi = rec
        .pointer("/pids/doi/identifier")
        .and_then(Value::as_str)
        .or_else(|| rec.get("doi").and_then(Value::as_str))
        .or_else(|| meta.get("doi").and_then(Value::as_str))
        .or_else(|| rec.pointer("/links/doi").and_then(Value::as_str))
        .and_then(normalize_doi);

    // Canonical landing page URL: links.self_html -> links.html -> doi_url -> links.doi -> https://doi.org/{doi} -> links.self
    let url = rec
        .pointer("/links/self_html")
        .and_then(Value::as_str)
        .or_else(|| rec.pointer("/links/html").and_then(Value::as_str))
        .or_else(|| rec.get("doi_url").and_then(Value::as_str))
        .or_else(|| rec.pointer("/links/doi").and_then(Value::as_str))
        .map(str::to_owned)
        .or_else(|| doi.as_ref().map(|d| format!("https://doi.org/{d}")))
        .or_else(|| {
            rec.pointer("/links/self")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();

    let published_date = meta
        .get("publication_date")
        .and_then(Value::as_str)
        .or_else(|| rec.get("created").and_then(Value::as_str))
        .and_then(normalize_date);
    let year = published_date.as_deref().and_then(year_from_date);

    let abstract_text = meta
        .get("description")
        .and_then(Value::as_str)
        .map(|s| clip(&strip_tags(s), 1200))
        .filter(|s| !s.is_empty());

    let venue = rec
        .pointer("/custom_fields/journal:journal/title")
        .and_then(Value::as_str)
        .or_else(|| {
            rec.pointer("/metadata/journal/title")
                .and_then(Value::as_str)
        })
        .or_else(|| meta.get("publisher").and_then(Value::as_str))
        .map(collapse_ws)
        .filter(|s| !s.is_empty());

    let raw_type = rec
        .pointer("/metadata/resource_type/id")
        .and_then(Value::as_str)
        .or_else(|| {
            rec.pointer("/metadata/resource_type/subtype")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            rec.pointer("/metadata/resource_type/type")
                .and_then(Value::as_str)
        });
    let work_type = normalize_work_type(raw_type);

    let mut work = ScholarlyWork::new(title, url.clone());
    work.authors = authors;
    work.year = year;
    work.published_date = published_date;
    work.venue = venue;
    work.work_type = work_type;
    work.ids.doi = doi;
    work.abstract_text = abstract_text;

    let is_open = if let Some(status) = rec.pointer("/access/status").and_then(Value::as_str) {
        status.eq_ignore_ascii_case("open")
    } else {
        let files_disabled = rec.pointer("/files/enabled").and_then(Value::as_bool) == Some(false);
        (!files_disabled
            && rec
                .pointer("/access/files")
                .and_then(Value::as_str)
                .is_some_and(|s| s.eq_ignore_ascii_case("public")))
            || meta
                .get("access_right")
                .and_then(Value::as_str)
                .is_some_and(|s| s.eq_ignore_ascii_case("open"))
    };
    if is_open {
        let mut oa = OaLocation::new("green");
        oa.url = (!url.is_empty()).then_some(url);
        oa.license = rec
            .pointer("/metadata/rights/0/id")
            .and_then(Value::as_str)
            .or_else(|| rec.pointer("/metadata/license/id").and_then(Value::as_str))
            .map(str::to_owned);
        oa.pdf_url = extract_pdf_url(rec);
        work.oa = Some(oa);
    }

    Some(work)
}

fn parse_creator(c: &Value) -> Option<AuthorRef> {
    if let Some(po) = c.get("person_or_org").and_then(Value::as_object) {
        let name = po
            .get("name")
            .and_then(Value::as_str)
            .map(collapse_ws)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                let given = po.get("given_name").and_then(Value::as_str).unwrap_or("");
                let family = po.get("family_name").and_then(Value::as_str).unwrap_or("");
                let full = collapse_ws(&format!("{given} {family}"));
                (!full.is_empty()).then_some(full)
            })?;
        let mut author = AuthorRef::new(name);
        if let Some(ids) = po.get("identifiers").and_then(Value::as_array) {
            for id in ids {
                if id
                    .get("scheme")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.eq_ignore_ascii_case("orcid"))
                    && let Some(val) = id.get("identifier").and_then(Value::as_str)
                {
                    author.orcid = Some(val.to_owned());
                    break;
                }
            }
        }
        author.affiliation = c
            .pointer("/affiliations/0/name")
            .and_then(Value::as_str)
            .map(collapse_ws)
            .filter(|s| !s.is_empty());
        return Some(author);
    }

    // Zenodo-v1-compat creator object: { "name": "...", "orcid": "...", "affiliation": "..." }
    let name = c
        .get("name")
        .and_then(Value::as_str)
        .map(collapse_ws)
        .filter(|s| !s.is_empty())?;
    let mut author = AuthorRef::new(name);
    author.orcid = c
        .get("orcid")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    author.affiliation = c
        .get("affiliation")
        .and_then(Value::as_str)
        .map(collapse_ws)
        .filter(|s| !s.is_empty());
    Some(author)
}

fn extract_pdf_url(rec: &Value) -> Option<String> {
    // Zenodo-v1 files array: [{ "key": "x.pdf", "links": { "self": "..." } }]
    if let Some(files) = rec.get("files").and_then(Value::as_array) {
        for f in files {
            let key = f.get("key").and_then(Value::as_str).unwrap_or("");
            if key.to_ascii_lowercase().ends_with(".pdf")
                && let Some(u) = f.pointer("/links/self").and_then(Value::as_str)
            {
                return Some(u.to_owned());
            }
        }
    }
    // Native InvenioRDM files.entries map
    if let Some(entries) = rec.pointer("/files/entries").and_then(Value::as_object) {
        for (key, entry) in entries {
            if key.to_ascii_lowercase().ends_with(".pdf")
                && let Some(u) = entry
                    .pointer("/links/content")
                    .or_else(|| entry.pointer("/links/self"))
                    .and_then(Value::as_str)
            {
                return Some(u.to_owned());
            }
        }
    }
    None
}

#[async_trait::async_trait]
impl ProtocolAdapter for InvenioRdmAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn ops(&self) -> &[OpDecl] {
        &self.ops
    }

    fn validate_dialect(&self, dialect: &BTreeMap<String, Value>) -> Result<(), FsmError> {
        reject_unknown_dialect_keys(KIND, dialect, DIALECT_KEYS)?;
        let _ = dialect_opt_str(KIND, dialect, "api_prefix")?;
        if let Some(comm) = dialect_opt_str(KIND, dialect, "community")? {
            let _ = encode_path_segment(&comm, "dialect.community")?;
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
            "record" => self.call_record(base_url, map, cx).await,
            other => Err(FsmError::invalid(format!(
                "unknown op {other:?} for protocol {KIND}"
            ))),
        }
    }
}
