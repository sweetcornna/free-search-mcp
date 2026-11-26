//! Dataverse Search & Dataset REST protocol adapter (`dataverse`, A1 §E #7 + Python `dataverse.py`).

use crate::common::{
    clip, collapse_ws, dialect_opt_str, normalize_date, opt_int, opt_limit, opt_str, params_map,
    parse_json, reject_unknown_dialect_keys, req_str, send_checked, strip_tags,
    trim_trailing_slash, validate_base_url,
};
use fsm_core::{
    Freshness, FsmError, HttpRequest, OpDecl, Param, ParamType, ProtocolAdapter, ProtocolOutput,
    ReturnKind, SourceDeps, SrcCx,
};
use jiff::{Span, Timestamp, tz::TimeZone};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

const KIND: &str = "dataverse";
const DIALECT_KEYS: &[&str] = &["default_type", "subtree"];

pub struct DataverseAdapter {
    deps: SourceDeps,
    ops: Vec<OpDecl>,
}

impl DataverseAdapter {
    pub fn new(deps: SourceDeps) -> Self {
        let ops = vec![
            OpDecl::new(
                "search",
                "Search a Dataverse repository for public research datasets.",
                ReturnKind::Records,
            )
            .with_param(Param::new("query", ParamType::String, "Search query").required())
            .with_param(
                Param::new("limit", ParamType::Int, "Maximum items per page")
                    .with_default(json!(10))
                    .with_range(1.0, 50.0),
            )
            .with_param(
                Param::new("start", ParamType::Int, "0-based pagination offset")
                    .with_range(0.0, 100_000.0),
            )
            .with_param(Param::new(
                "type",
                ParamType::String,
                "Dataverse object type (dataset, dataverse, file; defaults to dialect.default_type, else dataset)",
            ))
            .with_param(Param::new(
                "subtree",
                ParamType::String,
                "Optional Dataverse collection alias to scope results",
            ))
            .with_param(Param::new(
                "sort",
                ParamType::String,
                "Optional sort field (name or date)",
            )),
            OpDecl::new(
                "dataset",
                "Fetch detailed metadata for a Dataverse dataset by persistent ID (e.g. doi:10.7910/DVN/...).",
                ReturnKind::Records,
            )
            .with_param(
                Param::new(
                    "persistent_id",
                    ParamType::String,
                    "Dataset persistent identifier (e.g. doi:10.7910/DVN/...)",
                )
                .required(),
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
        let start = opt_int(params, "start", 0, 100_000)?;
        let dialect_type = dialect_opt_str(KIND, &cx.dialect, "default_type")?;
        let dialect_subtree = dialect_opt_str(KIND, &cx.dialect, "subtree")?;

        let item_type = opt_str(params, "type")?
            .or(dialect_type)
            .unwrap_or_else(|| "dataset".to_owned());
        let subtree = opt_str(params, "subtree")?.or(dialect_subtree);
        let sort = opt_str(params, "sort")?;

        let endpoint = format!("{}/api/search", trim_trailing_slash(base_url));
        let mut req = HttpRequest::get(endpoint)
            .query_param("q", &query)
            .query_param("type", &item_type)
            .query_param("per_page", limit.to_string())
            .header("Accept", "application/json");
        if let Some(st) = start {
            req = req.query_param("start", st.to_string());
        }
        if let Some(sub) = subtree {
            req = req.query_param("subtree", sub);
        }
        if let Some(s) = &sort {
            req = req.query_param("sort", s);
        }
        // Python dataverse.py parity: push freshness date filter when requested via SrcCx.
        if let Some(fresh) = cx.freshness {
            let days = match fresh {
                Freshness::Day => 1,
                Freshness::Week => 7,
                Freshness::Month => 31,
                Freshness::Year => 366,
                _ => 31,
            };
            let today_utc = Timestamp::now().to_zoned_datetime(TimeZone::UTC).date();
            if let Ok(since) = today_utc.checked_sub(Span::new().days(days)) {
                req = req.query_param("fq", format!("publicationDate:[{since} TO *]"));
                if sort.is_none() {
                    req = req.query_param("sort", "date").query_param("order", "desc");
                }
            }
        }

        let resp = send_checked(&self.deps, req, cx, KIND).await?;
        let val = parse_json(&resp, KIND)?;

        if let Some(status) = val.get("status").and_then(Value::as_str)
            && !status.eq_ignore_ascii_case("OK")
        {
            let msg = val
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Dataverse search error");
            return Err(FsmError::upstream(KIND, Some(resp.status), msg));
        }

        let data = val.get("data").and_then(Value::as_object).ok_or_else(|| {
            FsmError::upstream(
                KIND,
                Some(resp.status),
                "Dataverse search response missing data object",
            )
        })?;
        let raw_items = data.get("items").and_then(Value::as_array).ok_or_else(|| {
            FsmError::upstream(
                KIND,
                Some(resp.status),
                "Dataverse search response missing data.items array",
            )
        })?;

        let mut items = Vec::new();
        let mut consumed: usize = 0;
        for item in raw_items {
            if items.len() >= limit {
                break;
            }
            consumed += 1;
            if let Some(rec) = parse_search_item(item, base_url) {
                items.push(rec);
            }
        }

        let next_cursor = (|| {
            let cur_start = data.get("start").and_then(Value::as_u64).unwrap_or(0);
            let total = data.get("total_count").and_then(Value::as_u64)?;
            let next = cur_start.checked_add(consumed as u64)?;
            (next < total && consumed > 0).then(|| next.to_string())
        })();

        let mut out = ProtocolOutput::new(KIND, base_url, "search");
        out.items = items;
        out.next_cursor = next_cursor;
        Ok(out)
    }

    async fn call_dataset(
        &self,
        base_url: &str,
        params: &Map<String, Value>,
        cx: &SrcCx,
    ) -> Result<ProtocolOutput, FsmError> {
        let persistent_id = req_str(params, "persistent_id")?;
        let endpoint = format!(
            "{}/api/datasets/:persistentId/",
            trim_trailing_slash(base_url)
        );
        let req = HttpRequest::get(endpoint)
            .query_param("persistentId", &persistent_id)
            .header("Accept", "application/json");
        let resp = send_checked(&self.deps, req, cx, KIND).await?;
        let val = parse_json(&resp, KIND)?;

        if let Some(status) = val.get("status").and_then(Value::as_str)
            && !status.eq_ignore_ascii_case("OK")
        {
            let msg = val
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Dataverse dataset lookup error");
            return Err(FsmError::upstream(KIND, Some(resp.status), msg));
        }

        let data_obj = val.get("data").and_then(Value::as_object).ok_or_else(|| {
            FsmError::upstream(
                KIND,
                Some(resp.status),
                "Dataverse dataset response missing data object",
            )
        })?;
        if !data_obj.contains_key("latestVersion") && !data_obj.contains_key("id") {
            return Err(FsmError::upstream(
                KIND,
                Some(resp.status),
                "Dataverse dataset response missing data.latestVersion/id",
            ));
        }

        let mut items = Vec::new();
        if let Some(rec) = parse_dataset_detail(&val, base_url, &persistent_id) {
            items.push(rec);
        }

        let mut out = ProtocolOutput::new(KIND, base_url, "dataset");
        out.items = items;
        Ok(out)
    }
}

fn parse_search_item(item: &Value, base_url: &str) -> Option<Value> {
    let obj = item.as_object()?;
    let title = obj
        .get("name")
        .and_then(Value::as_str)
        .map(|s| clip(s, 300))
        .filter(|s| !s.is_empty())?;

    let global_id = obj
        .get("global_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let url = obj
        .get("url")
        .and_then(Value::as_str)
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        .map(str::to_owned)
        .or_else(|| {
            global_id.as_ref().map(|gid| {
                let encoded: String =
                    url::form_urlencoded::byte_serialize(gid.as_bytes()).collect();
                format!(
                    "{}/dataset.xhtml?persistentId={encoded}",
                    trim_trailing_slash(base_url)
                )
            })
        })?;

    let snippet = obj
        .get("description")
        .and_then(Value::as_str)
        .map(|s| clip(&strip_tags(s), 400))
        .unwrap_or_default();

    let date = obj
        .get("published_at")
        .and_then(Value::as_str)
        .and_then(normalize_date);

    let authors: Vec<Value> = obj
        .get("authors")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(collapse_ws)
                .filter(|s| !s.is_empty())
                .map(Value::String)
                .collect()
        })
        .unwrap_or_default();

    let item_type = obj
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("dataset")
        .to_owned();

    let mut rec = Map::new();
    rec.insert("title".into(), Value::String(title));
    rec.insert("url".into(), Value::String(url));
    rec.insert("snippet".into(), Value::String(snippet));
    if let Some(d) = date {
        rec.insert("date".into(), Value::String(d));
    }
    if let Some(gid) = global_id {
        rec.insert("globalId".into(), Value::String(gid));
    }
    rec.insert("authors".into(), Value::Array(authors));
    rec.insert("type".into(), Value::String(item_type));
    if let Some(pub_name) = obj
        .get("name_of_dataverse")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        rec.insert("publisher".into(), Value::String(pub_name.to_owned()));
    }
    Some(Value::Object(rec))
}

fn parse_dataset_detail(val: &Value, base_url: &str, persistent_id: &str) -> Option<Value> {
    let data = val.get("data").and_then(Value::as_object)?;
    let latest = data.get("latestVersion");
    let fields = latest
        .and_then(|lv| lv.pointer("/metadataBlocks/citation/fields"))
        .and_then(Value::as_array);

    let mut title = String::new();
    let mut description = String::new();
    let mut authors: Vec<Value> = Vec::new();

    if let Some(field_list) = fields {
        for field in field_list {
            let type_name = field.get("typeName").and_then(Value::as_str).unwrap_or("");
            match type_name {
                "title" => {
                    if let Some(s) = field.get("value").and_then(Value::as_str) {
                        title = collapse_ws(s);
                    }
                }
                "dsDescription" => {
                    if let Some(arr) = field.get("value").and_then(Value::as_array) {
                        for entry in arr {
                            if let Some(s) = entry
                                .pointer("/dsDescriptionValue/value")
                                .and_then(Value::as_str)
                            {
                                let clean = clip(&strip_tags(s), 1200);
                                if !clean.is_empty() {
                                    description = clean;
                                    break;
                                }
                            }
                        }
                    }
                }
                "author" => {
                    if let Some(arr) = field.get("value").and_then(Value::as_array) {
                        for entry in arr {
                            if let Some(name) =
                                entry.pointer("/authorName/value").and_then(Value::as_str)
                            {
                                let clean = collapse_ws(name);
                                if !clean.is_empty() {
                                    authors.push(Value::String(clean));
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    if title.is_empty() {
        title = persistent_id.to_owned();
    }

    let url = data
        .get("persistentUrl")
        .and_then(Value::as_str)
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        .map(str::to_owned)
        .unwrap_or_else(|| {
            let encoded: String =
                url::form_urlencoded::byte_serialize(persistent_id.as_bytes()).collect();
            format!(
                "{}/dataset.xhtml?persistentId={encoded}",
                trim_trailing_slash(base_url)
            )
        });

    let date = data
        .get("publicationDate")
        .and_then(Value::as_str)
        .or_else(|| {
            latest
                .and_then(|lv| lv.get("releaseTime"))
                .and_then(Value::as_str)
        })
        .and_then(normalize_date);

    let files: Vec<Value> = latest
        .and_then(|lv| lv.get("files"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    let df = f.get("dataFile").and_then(Value::as_object)?;
                    let filename = df.get("filename").and_then(Value::as_str)?;
                    let mut m = Map::new();
                    m.insert("filename".into(), Value::String(filename.to_owned()));
                    if let Some(ct) = df.get("contentType").and_then(Value::as_str) {
                        m.insert("contentType".into(), Value::String(ct.to_owned()));
                    }
                    if let Some(sz) = df.get("filesize").and_then(Value::as_u64) {
                        m.insert("filesize".into(), json!(sz));
                    }
                    Some(Value::Object(m))
                })
                .collect()
        })
        .unwrap_or_default();

    let mut rec = Map::new();
    rec.insert("title".into(), Value::String(title));
    rec.insert("url".into(), Value::String(url));
    rec.insert("snippet".into(), Value::String(description));
    if let Some(d) = date {
        rec.insert("date".into(), Value::String(d));
    }
    rec.insert("globalId".into(), Value::String(persistent_id.to_owned()));
    rec.insert("authors".into(), Value::Array(authors));
    rec.insert("type".into(), Value::String("dataset".to_owned()));
    if let Some(lic) = latest
        .and_then(|lv| lv.pointer("/license/name").or_else(|| lv.get("license")))
        .and_then(Value::as_str)
    {
        rec.insert("license".into(), Value::String(lic.to_owned()));
    }
    if !files.is_empty() {
        rec.insert("files".into(), Value::Array(files));
    }
    Some(Value::Object(rec))
}

#[async_trait::async_trait]
impl ProtocolAdapter for DataverseAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn ops(&self) -> &[OpDecl] {
        &self.ops
    }

    fn validate_dialect(&self, dialect: &BTreeMap<String, Value>) -> Result<(), FsmError> {
        reject_unknown_dialect_keys(KIND, dialect, DIALECT_KEYS)?;
        let _ = dialect_opt_str(KIND, dialect, "default_type")?;
        let _ = dialect_opt_str(KIND, dialect, "subtree")?;
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
            "dataset" => self.call_dataset(base_url, map, cx).await,
            other => Err(FsmError::invalid(format!(
                "unknown op {other:?} for protocol {KIND}"
            ))),
        }
    }
}
