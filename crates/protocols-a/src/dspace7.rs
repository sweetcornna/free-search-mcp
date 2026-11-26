//! DSpace 7/8 HAL-JSON REST protocol adapter (`dspace7`, A1 §E #5).

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
use url::Url;

const KIND: &str = "dspace7";
const DIALECT_KEYS: &[&str] = &["api_prefix", "scope_uuid", "dso_type"];
const DEFAULT_API_PREFIX: &str = "/server/api";

pub struct Dspace7Adapter {
    deps: SourceDeps,
    ops: Vec<OpDecl>,
}

impl Dspace7Adapter {
    pub fn new(deps: SourceDeps) -> Self {
        let ops = vec![
            OpDecl::new(
                "search",
                "Search a DSpace 7/8 repository via Discover HAL-JSON API.",
                ReturnKind::Works,
            )
            .with_param(Param::new("query", ParamType::String, "Search query").required())
            .with_param(
                Param::new("limit", ParamType::Int, "Maximum objects per page")
                    .with_default(json!(10))
                    .with_range(1.0, 50.0),
            )
            .with_param(
                Param::new("page", ParamType::Int, "0-based page number").with_range(0.0, 10_000.0),
            )
            .with_param(Param::new(
                "sort",
                ParamType::String,
                "Optional DSpace sort specification (e.g. dc.date.issued,DESC)",
            )),
            OpDecl::new(
                "item",
                "Fetch a single DSpace 7/8 item by UUID.",
                ReturnKind::Works,
            )
            .with_param(Param::new("id", ParamType::String, "DSpace item UUID").required()),
        ];
        Self { deps, ops }
    }

    async fn call_search(
        &self,
        base_url: &str,
        origin: &str,
        params: &Map<String, Value>,
        cx: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError> {
        let query = req_str(params, "query")?;
        let limit = opt_limit(params, "limit", 10, 1, 50)?;
        let page = opt_int(params, "page", 0, 10_000)?;
        let sort = opt_str(params, "sort")?;

        let api_prefix = effective_api_prefix(&cx.dialect)?;
        let scope_uuid = dialect_opt_str(KIND, &cx.dialect, "scope_uuid")?;
        let dso_type = dialect_opt_str(KIND, &cx.dialect, "dso_type")?;

        let url = format!(
            "{}{}/discover/search/objects",
            trim_trailing_slash(base_url),
            api_prefix
        );
        let mut req = HttpRequest::get(url)
            .query_param("query", &query)
            .query_param("size", limit.to_string())
            .header("Accept", "application/hal+json, application/json");
        if let Some(p) = page {
            req = req.query_param("page", p.to_string());
        }
        if let Some(s) = sort {
            req = req.query_param("sort", s);
        }
        if let Some(scope) = scope_uuid {
            req = req.query_param("scope", scope);
        }
        if let Some(dso) = dso_type {
            req = req.query_param("dsoType", dso);
        }

        let resp = send_checked(&self.deps, req, cx, KIND).await?;
        let val = parse_json(&resp, KIND)?;

        let search_result = val
            .pointer("/_embedded/searchResult")
            .filter(|v| v.is_object())
            .ok_or_else(|| {
                FsmError::upstream(
                    KIND,
                    Some(resp.status),
                    "DSpace 7 search response missing _embedded.searchResult object",
                )
            })?;
        let objects = search_result
            .pointer("/_embedded/objects")
            .and_then(Value::as_array);

        let mut items = Vec::new();
        if let Some(objs) = objects {
            for entry in objs {
                if items.len() >= limit {
                    break;
                }
                let indexable = entry
                    .pointer("/_embedded/indexableObject")
                    .or_else(|| entry.get("indexableObject"))
                    .unwrap_or(entry);
                if let Some(work) = parse_dspace_item(indexable, origin) {
                    items.push(work_to_value(work));
                }
            }
        }

        let next_cursor = search_result.get("page").and_then(|p| {
            let num = p.get("number")?.as_u64()?;
            let total_pages = p.get("totalPages")?.as_u64()?;
            num.checked_add(1)
                .filter(|&next| next < total_pages)
                .map(|next| next.to_string())
        });

        let mut out = ProtocolOutput::new(KIND, base_url, "search");
        out.items = items;
        out.next_cursor = next_cursor;
        Ok(out)
    }

    async fn call_item(
        &self,
        base_url: &str,
        origin: &str,
        params: &Map<String, Value>,
        cx: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError> {
        let id = req_str(params, "id")?;
        let encoded_id = encode_path_segment(&id, "id")?;
        let api_prefix = effective_api_prefix(&cx.dialect)?;

        let url = format!(
            "{}{}/core/items/{encoded_id}",
            trim_trailing_slash(base_url),
            api_prefix
        );
        let req = HttpRequest::get(url).header("Accept", "application/hal+json, application/json");
        let resp = send_checked(&self.deps, req, cx, KIND).await?;
        let val = parse_json(&resp, KIND)?;
        let obj = val.as_object().ok_or_else(|| {
            FsmError::upstream(
                KIND,
                Some(resp.status),
                "DSpace 7 item response is not a JSON object",
            )
        })?;
        if !obj.contains_key("metadata") && !obj.contains_key("id") && !obj.contains_key("uuid") {
            return Err(FsmError::upstream(
                KIND,
                Some(resp.status),
                "DSpace 7 item response missing metadata/id/uuid",
            ));
        }

        let mut items = Vec::new();
        if let Some(work) = parse_dspace_item(&val, origin) {
            items.push(work_to_value(work));
        }

        let mut out = ProtocolOutput::new(KIND, base_url, "item");
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

fn meta_values<'a>(meta: &'a Map<String, Value>, key: &str) -> Vec<&'a str> {
    meta.get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    v.get("value")
                        .and_then(Value::as_str)
                        .or_else(|| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                })
                .collect()
        })
        .unwrap_or_default()
}

fn first_meta<'a>(meta: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| meta_values(meta, k).into_iter().next())
}

fn parse_dspace_item(obj: &Value, origin: &str) -> Option<ScholarlyWork> {
    let map = obj.as_object()?;
    if let Some(dso_type) = map.get("type").and_then(Value::as_str)
        && !dso_type.eq_ignore_ascii_case("item")
    {
        return None;
    }
    let meta = map.get("metadata").and_then(Value::as_object);

    let title = meta
        .and_then(|m| first_meta(m, &["dc.title"]))
        .or_else(|| map.get("name").and_then(Value::as_str))
        .map(collapse_ws)
        .filter(|s| !s.is_empty())?;

    let mut authors = Vec::new();
    if let Some(m) = meta {
        for key in ["dc.contributor.author", "dc.creator", "dc.contributor"] {
            let vals = meta_values(m, key);
            if !vals.is_empty() {
                for name in vals {
                    let clean = collapse_ws(name);
                    if !clean.is_empty() && !authors.iter().any(|a: &AuthorRef| a.name == clean) {
                        authors.push(AuthorRef::new(clean));
                    }
                }
                break;
            }
        }
    }

    let published_date = meta
        .and_then(|m| {
            first_meta(
                m,
                &[
                    "dc.date.issued",
                    "dc.date.available",
                    "dc.date.created",
                    "dc.date",
                ],
            )
        })
        .and_then(normalize_date);
    let year = published_date.as_deref().and_then(year_from_date);

    let mut doi: Option<String> = None;
    let mut url: Option<String> = None;
    if let Some(m) = meta {
        for key in [
            "dc.identifier.doi",
            "dc.identifier.uri",
            "dc.identifier.other",
            "dc.identifier",
        ] {
            for val in meta_values(m, key) {
                if doi.is_none()
                    && let Some(d) = normalize_doi(val)
                {
                    doi = Some(d);
                }
                if url.is_none() && (val.starts_with("http://") || val.starts_with("https://")) {
                    url = Some(val.to_owned());
                }
            }
        }
    }

    if url.is_none() {
        if let Some(handle) = map
            .get("handle")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            url = Some(format!("{origin}/handle/{handle}"));
        } else if let Some(uuid) = map
            .get("uuid")
            .or_else(|| map.get("id"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            url = Some(format!("{origin}/items/{uuid}"));
        } else if let Some(href) = obj.pointer("/_links/self/href").and_then(Value::as_str) {
            url = Some(href.to_owned());
        }
    }

    let final_url = url
        .or_else(|| doi.as_ref().map(|d| format!("https://doi.org/{d}")))
        .unwrap_or_default();

    let abstract_text = meta
        .and_then(|m| first_meta(m, &["dc.description.abstract", "dc.description"]))
        .map(|s| clip(&strip_tags(s), 1200))
        .filter(|s| !s.is_empty());

    let venue = meta
        .and_then(|m| first_meta(m, &["dc.relation.ispartof", "dc.source", "dc.publisher"]))
        .map(collapse_ws)
        .filter(|s| !s.is_empty());

    let work_type = normalize_work_type(meta.and_then(|m| first_meta(m, &["dc.type"])));

    let mut work = ScholarlyWork::new(title, final_url.clone());
    work.authors = authors;
    work.year = year;
    work.published_date = published_date;
    work.venue = venue;
    work.work_type = work_type;
    work.ids.doi = doi;
    work.abstract_text = abstract_text;

    work.oa = meta.and_then(|m| detect_dspace_oa(m, &final_url));

    Some(work)
}

/// Detect open access only from positive rights/access metadata (never from
/// `inArchive`, which also covers restricted and embargoed theses).
fn detect_dspace_oa(meta: &Map<String, Value>, final_url: &str) -> Option<OaLocation> {
    if final_url.is_empty() {
        return None;
    }
    let mut is_open = false;
    let mut is_restricted = false;
    let mut license: Option<String> = None;

    for key in meta.keys() {
        let key_low = key.to_ascii_lowercase();
        let is_rights_or_version = (key_low.contains("rights")
            || key_low.contains("accesstype")
            || key_low == "dc.type"
            || key_low == "dc.description.version")
            && !key_low.contains("holder")
            && !key_low.contains("accessibility");
        if !is_rights_or_version {
            continue;
        }
        for val in meta_values(meta, key) {
            let low = val.to_ascii_lowercase();
            if crate::common::is_oa_denied(val) {
                is_restricted = true;
            }
            let has_cc = low.contains("creativecommons")
                || low.contains("cc-by")
                || low.contains("cc by")
                || low.contains("cc_by")
                || low.contains("ccby")
                || low.contains("cc0")
                || low.contains("public domain")
                || low.contains("publicdomain");
            let has_oa = has_cc
                || low.contains("openaccess")
                || low.contains("open access")
                || low.contains("open.access")
                || low.contains("acesso aberto")
                || low.contains("acceso abierto")
                || low.contains("acc\u{00E8}s ouvert")
                || low.contains("coar/access_right/c_abf2");
            if has_oa {
                is_open = true;
                if license.is_none() && has_cc {
                    license = Some(val.to_owned());
                }
            }
        }
    }

    if !is_open || is_restricted {
        return None;
    }
    if license.is_none() {
        license = first_meta(
            meta,
            &[
                "dc.rights.ccuri",
                "dc.rights.uri",
                "dc.rights.license",
                "dc.rights",
            ],
        )
        .map(str::to_owned);
    }
    let mut oa = OaLocation::new("green");
    oa.url = Some(final_url.to_owned());
    oa.license = license;
    Some(oa)
}

fn origin_of(url: &Url) -> String {
    url.origin().ascii_serialization()
}

#[async_trait::async_trait]
impl ProtocolAdapter for Dspace7Adapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn ops(&self) -> &[OpDecl] {
        &self.ops
    }

    fn validate_dialect(&self, dialect: &BTreeMap<String, Value>) -> Result<(), FsmError> {
        reject_unknown_dialect_keys(KIND, dialect, DIALECT_KEYS)?;
        let _ = dialect_opt_str(KIND, dialect, "api_prefix")?;
        let _ = dialect_opt_str(KIND, dialect, "scope_uuid")?;
        let _ = dialect_opt_str(KIND, dialect, "dso_type")?;
        Ok(())
    }

    async fn call(
        &self,
        base_url: &str,
        op: &str,
        params: Value,
        cx: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError> {
        let parsed = validate_base_url(base_url)?;
        let origin = origin_of(&parsed);
        self.validate_dialect(&cx.dialect)?;
        let map = params_map(&params)?;
        match op {
            "search" => self.call_search(base_url, &origin, map, cx).await,
            "item" => self.call_item(base_url, &origin, map, cx).await,
            other => Err(FsmError::invalid(format!(
                "unknown op {other:?} for protocol {KIND}"
            ))),
        }
    }
}
