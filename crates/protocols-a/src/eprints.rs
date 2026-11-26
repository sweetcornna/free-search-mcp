//! EPrints JSON Export protocol adapter (`eprints`, A1 §E #8).

use crate::common::{
    clip, collapse_ws, decode_body, dialect_opt_bool, dialect_opt_i64, dialect_opt_str,
    normalize_date, normalize_doi, normalize_work_type, opt_limit, params_map,
    reject_unknown_dialect_keys, req_str, send_checked, strip_tags, trim_trailing_slash,
    validate_base_url, work_to_value, year_from_date,
};
use fsm_core::{
    AuthorRef, FsmError, HttpRequest, OaLocation, OpDecl, Param, ParamType, ProtocolAdapter,
    ProtocolOutput, ReturnKind, ScholarlyWork, SourceDeps, SrcCx,
};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

const KIND: &str = "eprints";
const DIALECT_KEYS: &[&str] = &["max_response_bytes", "multilingual_title", "search_path"];
const DEFAULT_MAX_BYTES: i64 = 2_097_152;
const MIN_MAX_BYTES: i64 = 65_536;
const MAX_MAX_BYTES: i64 = 10_485_760;
const DEFAULT_SEARCH_PATH: &str = "/cgi/search/simple";

pub struct EprintsAdapter {
    deps: SourceDeps,
    ops: Vec<OpDecl>,
}

impl EprintsAdapter {
    pub fn new(deps: SourceDeps) -> Self {
        let ops = vec![
            OpDecl::new(
                "search",
                "Search an EPrints repository via /cgi/search/simple?output=JSON (bounded read, client-sliced).",
                ReturnKind::Works,
            )
            .with_param(Param::new("query", ParamType::String, "Search query").required())
            .with_param(
                Param::new("limit", ParamType::Int, "Maximum works to return (client-sliced)")
                    .with_default(json!(10))
                    .with_range(1.0, 50.0),
            ),
            OpDecl::new(
                "record",
                "Fetch a single EPrints record JSON by numeric eprintid.",
                ReturnKind::Works,
            )
            .with_param(
                Param::new("id", ParamType::String, "Numeric EPrints eprintid").required(),
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
        let query = req_str(params, "query")?;
        let limit = opt_limit(params, "limit", 10, 1, 50)?;
        let max_bytes = dialect_opt_i64(
            KIND,
            &cx.dialect,
            "max_response_bytes",
            MIN_MAX_BYTES,
            MAX_MAX_BYTES,
        )?
        .unwrap_or(DEFAULT_MAX_BYTES) as u64;
        let multilingual_title =
            dialect_opt_bool(KIND, &cx.dialect, "multilingual_title")?.unwrap_or(true);
        let search_path = effective_search_path(&cx.dialect)?;

        let url = format!("{}{}", trim_trailing_slash(base_url), search_path);
        let req = HttpRequest::get(url)
            .query_param("q", &query)
            .query_param("output", "JSON")
            .header("Accept", "application/json")
            .with_max_bytes(max_bytes);

        let resp = send_checked(&self.deps, req, cx, KIND).await?;
        let text = decode_body(&resp);
        reject_html_fallback(&resp, &text)?;

        let records = parse_eprints_array(&text, resp.truncated, resp.status)?;
        let mut items = Vec::new();
        for rec in records {
            if items.len() >= limit {
                break;
            }
            if let Some(work) = parse_eprints_record(&rec, base_url, multilingual_title) {
                items.push(work_to_value(work));
            }
        }

        let mut out = ProtocolOutput::new(KIND, base_url, "search");
        out.items = items;
        Ok(out)
    }

    async fn call_record(
        &self,
        base_url: &str,
        params: &Map<String, Value>,
        cx: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError> {
        let id = req_str(params, "id")?;
        if !id.chars().all(|c| c.is_ascii_digit()) {
            return Err(FsmError::invalid(format!(
                "{KIND}: parameter \"id\" must be a numeric eprintid, got {id:?}"
            )));
        }
        let max_bytes = dialect_opt_i64(
            KIND,
            &cx.dialect,
            "max_response_bytes",
            MIN_MAX_BYTES,
            MAX_MAX_BYTES,
        )?
        .unwrap_or(DEFAULT_MAX_BYTES) as u64;
        let multilingual_title =
            dialect_opt_bool(KIND, &cx.dialect, "multilingual_title")?.unwrap_or(true);

        let url = format!(
            "{}/cgi/export/eprint/{id}/JSON/eprint-{id}.js",
            trim_trailing_slash(base_url)
        );
        let req = HttpRequest::get(url)
            .header("Accept", "application/json")
            .with_max_bytes(max_bytes);

        let resp = send_checked(&self.deps, req, cx, KIND).await?;
        let text = decode_body(&resp);
        reject_html_fallback(&resp, &text)?;

        let val: Value = serde_json::from_str(&text).map_err(|e| {
            FsmError::upstream(
                KIND,
                Some(resp.status),
                format!("invalid EPrints JSON record ({e})"),
            )
        })?;

        let mut items = Vec::new();
        match val {
            Value::Object(_) => {
                if let Some(work) = parse_eprints_record(&val, base_url, multilingual_title) {
                    items.push(work_to_value(work));
                }
            }
            Value::Array(arr) => {
                if let Some(first) = arr.first()
                    && let Some(work) = parse_eprints_record(first, base_url, multilingual_title)
                {
                    items.push(work_to_value(work));
                }
            }
            _ => {
                return Err(FsmError::upstream(
                    KIND,
                    Some(resp.status),
                    "EPrints record endpoint did not return a JSON object or array",
                ));
            }
        }

        let mut out = ProtocolOutput::new(KIND, base_url, "record");
        out.items = items;
        Ok(out)
    }
}

fn effective_search_path(dialect: &BTreeMap<String, Value>) -> Result<String, FsmError> {
    let raw = dialect_opt_str(KIND, dialect, "search_path")?
        .unwrap_or_else(|| DEFAULT_SEARCH_PATH.to_owned());
    if raw.starts_with('/') {
        Ok(raw)
    } else {
        Ok(format!("/{raw}"))
    }
}

fn reject_html_fallback(resp: &fsm_core::HttpBytes, text: &str) -> Result<(), FsmError> {
    let is_html_ct = resp.media_type().is_some_and(|ct| ct.contains("html"));
    let trimmed = text.trim_start();
    if is_html_ct || trimmed.starts_with('<') {
        return Err(FsmError::upstream(
            KIND,
            Some(resp.status),
            "EPrints returned HTML instead of JSON (output=JSON disabled or WAF page)",
        ));
    }
    Ok(())
}

fn parse_eprints_array(text: &str, truncated: bool, status: u16) -> Result<Vec<Value>, FsmError> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(arr)) => Ok(arr),
        Ok(_) => Err(FsmError::upstream(
            KIND,
            Some(status),
            "EPrints search response is not a JSON array",
        )),
        Err(e) if truncated => {
            let salvaged = salvage_truncated_json_array(text);
            if !salvaged.is_empty() {
                Ok(salvaged)
            } else {
                Err(FsmError::upstream(
                    KIND,
                    Some(status),
                    format!("truncated EPrints JSON could not be parsed ({e})"),
                ))
            }
        }
        Err(e) => Err(FsmError::upstream(
            KIND,
            Some(status),
            format!("invalid EPrints JSON ({e})"),
        )),
    }
}

/// When EPrints output is bounded by `max_response_bytes` mid-array, extract all
/// complete top-level JSON objects from the start of the `[...` buffer.
fn salvage_truncated_json_array(text: &str) -> Vec<Value> {
    let trimmed = text.trim_start();
    let Some(rest) = trimmed.strip_prefix('[') else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escape = false;
    let mut obj_start: Option<usize> = None;

    for (idx, ch) in rest.char_indices() {
        if in_string {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => {
                if depth == 0 {
                    obj_start = Some(idx);
                }
                depth += 1;
            }
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0
                    && let Some(start) = obj_start.take()
                    && let Ok(v) = serde_json::from_str::<Value>(&rest[start..=idx])
                {
                    out.push(v);
                }
            }
            _ => {}
        }
    }
    out
}

pub(crate) fn parse_eprints_record(
    rec: &Value,
    base_url: &str,
    multilingual_title: bool,
) -> Option<ScholarlyWork> {
    let obj = rec.as_object()?;
    let title = extract_eprints_text(obj.get("title")?, multilingual_title)?;

    // Authors: creators[].name.{given, family} -> fallback rioxx2_author[].author
    let mut authors = Vec::new();
    if let Some(creators) = obj.get("creators").and_then(Value::as_array) {
        for c in creators {
            if let Some(a) = parse_eprints_creator(c) {
                authors.push(a);
            }
        }
    }
    if authors.is_empty()
        && let Some(rioxx) = obj.get("rioxx2_author").and_then(Value::as_array)
    {
        for c in rioxx {
            if let Some(name) = c
                .get("author")
                .and_then(Value::as_str)
                .map(collapse_ws)
                .filter(|s| !s.is_empty())
            {
                authors.push(AuthorRef::new(name));
            }
        }
    }

    // Date may be a string ("2016-05-02", "2024-07") OR an integer year (2014 on LMU/UTAS).
    let raw_date = obj.get("date").and_then(|v| {
        v.as_str()
            .map(str::to_owned)
            .or_else(|| v.as_i64().map(|n| n.to_string()))
            .or_else(|| v.as_u64().map(|n| n.to_string()))
    });
    let published_date = raw_date.as_deref().and_then(normalize_date);
    let year = published_date.as_deref().and_then(year_from_date);

    let doi = obj
        .get("doi")
        .and_then(Value::as_str)
        .and_then(normalize_doi)
        .or_else(|| {
            obj.get("id_number")
                .and_then(Value::as_str)
                .and_then(normalize_doi)
        });

    let url = obj
        .get("uri")
        .and_then(Value::as_str)
        .or_else(|| obj.get("official_url").and_then(Value::as_str))
        .or_else(|| obj.get("identifierabstract").and_then(Value::as_str))
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        .map(str::to_owned)
        .or_else(|| {
            obj.get("eprintid")
                .and_then(|v| {
                    v.as_u64()
                        .map(|n| n.to_string())
                        .or_else(|| v.as_str().map(str::to_owned))
                })
                .map(|eid| format!("{}/{eid}/", trim_trailing_slash(base_url)))
        })
        .or_else(|| doi.as_ref().map(|d| format!("https://doi.org/{d}")))
        .unwrap_or_default();

    let venue = obj
        .get("publication")
        .or_else(|| obj.get("book_title"))
        .or_else(|| obj.get("event_title"))
        .or_else(|| obj.get("publisher"))
        .and_then(Value::as_str)
        .map(collapse_ws)
        .filter(|s| !s.is_empty());

    let abstract_text = obj
        .get("abstract")
        .and_then(|v| extract_eprints_text(v, multilingual_title))
        .map(|s| clip(&strip_tags(&s), 1200))
        .filter(|s| !s.is_empty());

    let work_type = normalize_work_type(obj.get("type").and_then(Value::as_str));

    let mut work = ScholarlyWork::new(title, url.clone());
    work.authors = authors;
    work.year = year;
    work.published_date = published_date;
    work.venue = venue;
    work.work_type = work_type;
    work.ids.doi = doi;
    work.abstract_text = abstract_text;

    if let Some(oa) = extract_eprints_oa(obj, &url) {
        work.oa = Some(oa);
    }

    Some(work)
}

/// Handle both scalar string values and EPrints multilingual arrays
/// (`[{"name": "...", "lang": "eng"}]`), preferring `eng`/`en` else first entry.
fn extract_eprints_text(val: &Value, allow_multilingual: bool) -> Option<String> {
    match val {
        Value::String(s) => {
            let clean = collapse_ws(s);
            (!clean.is_empty()).then_some(clean)
        }
        Value::Array(arr) if allow_multilingual => {
            let mut first_any: Option<String> = None;
            for entry in arr {
                let (text, lang) = match entry {
                    Value::String(s) => (collapse_ws(s), None),
                    Value::Object(m) => {
                        let t = m
                            .get("name")
                            .or_else(|| m.get("value"))
                            .and_then(Value::as_str)
                            .map(collapse_ws)
                            .unwrap_or_default();
                        let l = m.get("lang").and_then(Value::as_str);
                        (t, l)
                    }
                    _ => continue,
                };
                if text.is_empty() {
                    continue;
                }
                if lang.is_some_and(|l| {
                    let ll = l.to_ascii_lowercase();
                    ll == "eng" || ll == "en" || ll.starts_with("en-")
                }) {
                    return Some(text);
                }
                if first_any.is_none() {
                    first_any = Some(text);
                }
            }
            first_any
        }
        _ => None,
    }
}

fn parse_eprints_creator(c: &Value) -> Option<AuthorRef> {
    let name = match c.get("name") {
        Some(Value::String(s)) => collapse_ws(s),
        Some(Value::Object(m)) => {
            let given = m.get("given").and_then(Value::as_str).unwrap_or("");
            let family = m.get("family").and_then(Value::as_str).unwrap_or("");
            collapse_ws(&format!("{given} {family}"))
        }
        _ => c.as_str().map(collapse_ws).unwrap_or_default(),
    };
    if name.is_empty() {
        return None;
    }
    let mut author = AuthorRef::new(name);
    author.orcid = c
        .get("orcid")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Some(author)
}

fn extract_eprints_oa(obj: &Map<String, Value>, landing_url: &str) -> Option<OaLocation> {
    let docs = obj.get("documents").and_then(Value::as_array);
    let mut pdf_url: Option<String> = None;
    let mut license: Option<String> = None;

    if let Some(doc_list) = docs {
        for doc in doc_list {
            let sec = doc
                .get("security")
                .and_then(Value::as_str)
                .unwrap_or("public");
            if !sec.eq_ignore_ascii_case("public") {
                continue;
            }
            if license.is_none() {
                license = doc
                    .get("license")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
            }
            let is_pdf = doc
                .get("mime_type")
                .and_then(Value::as_str)
                .is_some_and(|m| m.eq_ignore_ascii_case("application/pdf"))
                || doc
                    .get("format")
                    .and_then(Value::as_str)
                    .is_some_and(|m| m.eq_ignore_ascii_case("application/pdf"))
                || doc
                    .pointer("/files/0/mime_type")
                    .and_then(Value::as_str)
                    .is_some_and(|m| m.eq_ignore_ascii_case("application/pdf"))
                || doc
                    .get("main")
                    .and_then(Value::as_str)
                    .is_some_and(|m| m.to_ascii_lowercase().ends_with(".pdf"));
            if is_pdf && pdf_url.is_none() {
                pdf_url = doc
                    .pointer("/files/0/uri")
                    .and_then(Value::as_str)
                    .or_else(|| doc.get("uri").and_then(Value::as_str))
                    .map(str::to_owned);
            }
        }
    }

    let oa_flag = obj
        .get("openaccess")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| {
            !s.is_empty()
                && !matches!(
                    s.to_ascii_lowercase().as_str(),
                    "no" | "false" | "0" | "closed" | "restricted" | "none" | "embargoed"
                )
        });
    let ft_public = obj
        .get("full_text_status")
        .and_then(Value::as_str)
        .is_some_and(|s| s.eq_ignore_ascii_case("public"));

    if pdf_url.is_none() && oa_flag.is_none() && !ft_public {
        return None;
    }

    let status = oa_flag
        .filter(|s| !s.eq_ignore_ascii_case("yes") && !s.eq_ignore_ascii_case("true"))
        .unwrap_or("green")
        .to_ascii_lowercase();
    let mut oa = OaLocation::new(status);
    oa.url = (!landing_url.is_empty()).then(|| landing_url.to_owned());
    oa.pdf_url = pdf_url;
    oa.license = license;
    Some(oa)
}

#[async_trait::async_trait]
impl ProtocolAdapter for EprintsAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn ops(&self) -> &[OpDecl] {
        &self.ops
    }

    fn validate_dialect(&self, dialect: &BTreeMap<String, Value>) -> Result<(), FsmError> {
        reject_unknown_dialect_keys(KIND, dialect, DIALECT_KEYS)?;
        let _ = dialect_opt_i64(
            KIND,
            dialect,
            "max_response_bytes",
            MIN_MAX_BYTES,
            MAX_MAX_BYTES,
        )?;
        let _ = dialect_opt_bool(KIND, dialect, "multilingual_title")?;
        let _ = dialect_opt_str(KIND, dialect, "search_path")?;
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
