//! `HostServices` implementation wiring every non-prelude `HOST_API` entry to [`Services`],
//! enforcing centralized M6 `egress` taint control, argument validation, offline cache SSRF
//! checks, and a `catch_unwind` boundary (`SPEC.md` §3.1–§3.4).

use async_trait::async_trait;
use fsm_core::{
    AskOpts, CallCx, CatalogQuery, CiteInput, CiteStyle, EntityKind, FetchOpts, FetchOutcome,
    FsmError, FulltextOpts, GraphOpts, HostServices, HostValue, HttpMethod, RawRequestOpts,
    RenderKind, ResearchOpts, ScholarQuery, SearchRequest, Services,
};
use fsm_store::{
    EXEC_KV_DEFAULT_TTL_S, EXEC_KV_MAX_TTL_S, EXEC_KV_SHARED_SESSION, validate_user_kv_key,
};
use futures::FutureExt;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::panic::AssertUnwindSafe;
use std::time::Duration;

/// Exact set of async host functions dispatched by [`DefaultHostServices`].
///
/// Bijection tests verify this list equals all non-`is_prelude_sync` entries in
/// `fsm_surface::HOST_API`. (`util.sleep` is intercepted natively by `fsm-script`
/// when running inside the sandbox, and is also implemented here for direct callers.)
pub const DISPATCHED_HOST_FNS: &[&str] = &[
    // web (9)
    "web.ask",
    "web.search",
    "web.fetch",
    "web.fetchBatch",
    "web.request",
    "web.structured",
    "web.research",
    "web.compare",
    "web.download",
    // scholar (10)
    "scholar.search",
    "scholar.work",
    "scholar.graph",
    "scholar.fulltext",
    "scholar.integrity",
    "scholar.cite",
    "scholar.author",
    "scholar.org",
    "scholar.venue",
    "scholar.grants",
    // doc (2)
    "doc.read",
    "doc.outline",
    // source (2)
    "source.call",
    "source.protocol",
    // cache (4)
    "cache.search",
    "cache.getPage",
    "cache.put",
    "cache.get",
    // catalog (2)
    "catalog.search",
    "catalog.describe",
    // util (2 async; 4 prelude-sync helpers run inside QuickJS)
    "util.sleep",
    "util.htmlToMd",
];

/// Production [`HostServices`] backed by [`Services`].
#[derive(Clone, Debug)]
pub struct DefaultHostServices {
    services: Services,
}

impl DefaultHostServices {
    pub fn new(services: Services) -> Self {
        Self { services }
    }

    pub fn services(&self) -> &Services {
        &self.services
    }

    async fn dispatch(&self, func: &str, args: Value, cx: &CallCx) -> Result<HostValue, FsmError> {
        let Some(hf) = fsm_surface::host_fn(func) else {
            return Err(FsmError::invalid(format!(
                "unknown host function {func:?}; inspect available APIs via catalog.search(...)"
            )));
        };
        if hf.is_prelude_sync {
            return Err(FsmError::invalid(format!(
                "{func} is a synchronous prelude helper and cannot be invoked asynchronously"
            )));
        }

        // Centralized M6 local-document taint rule: once this exec has touched a local
        // document under DOCUMENT_ROOT, refuse ALL `egress = true` calls except:
        //   (a) local `doc.read` / `doc.outline` (non-URL paths), and
        //   (b) GET `web.fetch` / `web.fetchBatch` targeting hosts already in `cx.fetched_hosts`.
        if cx.is_tainted() && hf.egress {
            check_tainted_egress(func, &args, cx).await?;
        }

        let arr = match args {
            Value::Array(a) => a,
            Value::Null => Vec::new(),
            other => vec![other],
        };

        match func {
            // ---- web ----
            "web.ask" => self.web_ask(&arr, cx).await,
            "web.search" => self.web_search(&arr, cx).await,
            "web.fetch" => self.web_fetch(&arr, cx).await,
            "web.fetchBatch" => self.web_fetch_batch(&arr, cx).await,
            "web.request" => self.web_request(&arr, cx).await,
            "web.structured" => self.web_structured(&arr, cx).await,
            "web.research" => self.web_research(&arr, cx).await,
            "web.compare" => self.web_compare(&arr, cx).await,
            "web.download" => self.web_download(&arr, cx).await,
            // ---- scholar ----
            "scholar.search" => self.scholar_search(&arr, cx).await,
            "scholar.work" => self.scholar_work(&arr, cx).await,
            "scholar.graph" => self.scholar_graph(&arr, cx).await,
            "scholar.fulltext" => self.scholar_fulltext(&arr, cx).await,
            "scholar.integrity" => self.scholar_integrity(&arr, cx).await,
            "scholar.cite" => self.scholar_cite(&arr, cx).await,
            "scholar.author" => self.scholar_entity(EntityKind::Author, &arr, cx).await,
            "scholar.org" => self.scholar_entity(EntityKind::Org, &arr, cx).await,
            "scholar.venue" => self.scholar_entity(EntityKind::Venue, &arr, cx).await,
            "scholar.grants" => self.scholar_entity(EntityKind::Grants, &arr, cx).await,
            // ---- doc ----
            "doc.read" => self.doc_read(&arr, cx).await,
            "doc.outline" => self.doc_outline(&arr, cx).await,
            // ---- source ----
            "source.call" => self.source_call(&arr, cx).await,
            "source.protocol" => self.source_protocol(&arr, cx).await,
            // ---- cache ----
            "cache.search" => self.cache_search(&arr).await,
            "cache.getPage" => self.cache_get_page(&arr).await,
            "cache.put" => self.cache_put(&arr, cx).await,
            "cache.get" => self.cache_get(&arr, cx).await,
            // ---- catalog ----
            "catalog.search" => self.catalog_search(&arr).await,
            "catalog.describe" => self.catalog_describe(&arr).await,
            // ---- util ----
            "util.sleep" => self.util_sleep(&arr, cx).await,
            "util.htmlToMd" => self.util_html_to_md(&arr).await,
            other => Err(FsmError::internal(
                "host_dispatch_missing",
                format!("HOST_API entry {other:?} has no HostServices match arm"),
            )),
        }
    }

    async fn web_ask(&self, args: &[Value], cx: &CallCx) -> Result<HostValue, FsmError> {
        let question = req_str(args, 0, "web.ask", "question")?;
        let opts: AskOpts = opt_struct(args, 1, "web.ask", "opts")?;
        let Some(ask) = &self.services.ask else {
            return Err(FsmError::internal(
                "ask_missing",
                "ask service is not wired into Services",
            ));
        };
        let resp = ask.ask(&question, opts, cx).await?;
        Ok(HostValue::tagged(to_json(resp)?, RenderKind::AskResponse))
    }

    async fn web_search(&self, args: &[Value], cx: &CallCx) -> Result<HostValue, FsmError> {
        let query = req_str(args, 0, "web.search", "query")?;
        let mut req: SearchRequest = opt_struct(args, 1, "web.search", "opts")?;
        req.query = query;
        let resp = self.services.search.search(req, cx).await?;
        Ok(HostValue::tagged(
            to_json(resp)?,
            RenderKind::SearchResponse,
        ))
    }

    async fn web_fetch(&self, args: &[Value], cx: &CallCx) -> Result<HostValue, FsmError> {
        let url = req_str(args, 0, "web.fetch", "url")?;
        let opts: FetchOpts = opt_struct(args, 1, "web.fetch", "opts")?;
        match self.services.fetch.fetch(&url, opts, cx).await? {
            FetchOutcome::Page(page) => {
                Ok(HostValue::tagged(to_json(page)?, RenderKind::PageResult))
            }
            FetchOutcome::InlineImage { meta, binary } => {
                Ok(HostValue::tagged(to_json(meta)?, RenderKind::InlineImage).with_binary(binary))
            }
            _ => Err(FsmError::internal(
                "fetch_outcome",
                "unsupported FetchOutcome variant",
            )),
        }
    }

    async fn web_fetch_batch(&self, args: &[Value], cx: &CallCx) -> Result<HostValue, FsmError> {
        let urls: Vec<String> = req_typed(args, 0, "web.fetchBatch", "urls")?;
        let mut max_concurrency: u8 = 8;
        let opts: FetchOpts = match args.get(1) {
            None | Some(Value::Null) => FetchOpts::default(),
            Some(Value::Object(obj)) => {
                if let Some(mc) = obj.get("maxConcurrency").filter(|v| !v.is_null()) {
                    let n = mc.as_u64().ok_or_else(|| {
                        FsmError::invalid(
                            "web.fetchBatch: opts.maxConcurrency must be a positive integer",
                        )
                    })?;
                    max_concurrency = n.clamp(1, 8) as u8;
                }
                let mut clean = obj.clone();
                clean.remove("maxConcurrency");
                serde_json::from_value(Value::Object(clean))
                    .map_err(|e| FsmError::invalid(format!("web.fetchBatch: invalid opts: {e}")))?
            }
            Some(_) => {
                return Err(FsmError::invalid("web.fetchBatch: opts must be an object"));
            }
        };
        let pages = self
            .services
            .fetch
            .fetch_batch(urls, opts, max_concurrency, cx)
            .await?;
        Ok(HostValue::tagged(to_json(pages)?, RenderKind::PageList))
    }

    async fn web_request(&self, args: &[Value], cx: &CallCx) -> Result<HostValue, FsmError> {
        let raw = args.first().ok_or_else(|| {
            FsmError::invalid(
                "web.request requires an options object `{url, method?, headers?, query?, body?}`",
            )
        })?;
        let opts: RawRequestOpts = match raw {
            Value::Object(obj) => {
                let mut normalized = obj.clone();
                if let Some(Value::String(m)) = normalized.get("method") {
                    let upper = m.trim().to_ascii_uppercase();
                    if !matches!(upper.as_str(), "GET" | "HEAD" | "POST") {
                        return Err(FsmError::invalid(format!(
                            "web.request: unsupported HTTP method {m:?}; only GET, HEAD, and POST are allowed"
                        )));
                    }
                    normalized.insert("method".to_owned(), Value::String(upper));
                }
                let opts: RawRequestOpts = serde_json::from_value(Value::Object(normalized))
                    .map_err(|e| FsmError::invalid(format!("web.request: invalid options: {e}")))?;
                if opts.url.trim().is_empty() {
                    return Err(FsmError::invalid("web.request: url must not be empty"));
                }
                opts
            }
            _ => {
                return Err(FsmError::invalid(
                    "web.request expects an options object `{url, method?, headers?, query?, body?}`",
                ));
            }
        };
        if let Some(m) = opts.method
            && !matches!(m, HttpMethod::Get | HttpMethod::Head | HttpMethod::Post)
        {
            return Err(FsmError::invalid(
                "web.request: only GET, HEAD, and POST are allowed",
            ));
        }
        let resp = self.services.fetch.request(opts, cx).await?;
        Ok(HostValue::new(to_json(resp)?))
    }

    async fn web_structured(&self, args: &[Value], cx: &CallCx) -> Result<HostValue, FsmError> {
        let url = req_str(args, 0, "web.structured", "url")?;
        let meta = self.services.fetch.structured(&url, cx).await?;
        Ok(HostValue::new(to_json(meta)?))
    }

    async fn web_research(&self, args: &[Value], cx: &CallCx) -> Result<HostValue, FsmError> {
        let query = req_str(args, 0, "web.research", "query")?;
        let opts: ResearchOpts = opt_struct(args, 1, "web.research", "opts")?;
        let brief = self.services.fetch.research(&query, opts, cx).await?;
        Ok(HostValue::tagged(
            to_json(brief)?,
            RenderKind::ResearchBrief,
        ))
    }

    async fn web_compare(&self, args: &[Value], cx: &CallCx) -> Result<HostValue, FsmError> {
        let query = req_str(args, 0, "web.compare", "query")?;
        let urls: Vec<String> = req_typed(args, 1, "web.compare", "urls")?;
        let items = self.services.fetch.compare(&query, urls, cx).await?;
        Ok(HostValue::new(to_json(items)?))
    }

    async fn web_download(&self, args: &[Value], cx: &CallCx) -> Result<HostValue, FsmError> {
        let url = req_str(args, 0, "web.download", "url")?;
        let dl = self.services.fetch.download(&url, cx).await?;
        Ok(HostValue::new(to_json(dl)?))
    }

    async fn scholar_search(&self, args: &[Value], cx: &CallCx) -> Result<HostValue, FsmError> {
        let query = req_str(args, 0, "scholar.search", "query")?;
        let mut sq: ScholarQuery = opt_struct(args, 1, "scholar.search", "opts")?;
        sq.query = query;
        let res = self.services.scholar.search(sq, cx).await?;
        Ok(HostValue::tagged(to_json(res)?, RenderKind::WorkResult))
    }

    async fn scholar_work(&self, args: &[Value], cx: &CallCx) -> Result<HostValue, FsmError> {
        let id = req_str(args, 0, "scholar.work", "idOrTitle")?;
        match self.services.scholar.work(&id, cx).await? {
            Some(work) => Ok(HostValue::tagged(to_json(work)?, RenderKind::WorkList)),
            None => Ok(HostValue::new(Value::Null)),
        }
    }

    async fn scholar_graph(&self, args: &[Value], cx: &CallCx) -> Result<HostValue, FsmError> {
        let id = req_str(args, 0, "scholar.graph", "idOrTitle")?;
        let opts: GraphOpts = opt_struct(args, 1, "scholar.graph", "opts")?;
        let g = self.services.scholar.graph(&id, opts, cx).await?;
        Ok(HostValue::tagged(to_json(g)?, RenderKind::CitationGraph))
    }

    async fn scholar_fulltext(&self, args: &[Value], cx: &CallCx) -> Result<HostValue, FsmError> {
        let id = req_str(args, 0, "scholar.fulltext", "idOrDoi")?;
        let opts: FulltextOpts = opt_struct(args, 1, "scholar.fulltext", "opts")?;
        let ft = self.services.scholar.fulltext(&id, opts, cx).await?;
        Ok(HostValue::tagged(to_json(ft)?, RenderKind::FulltextResult))
    }

    async fn scholar_integrity(&self, args: &[Value], cx: &CallCx) -> Result<HostValue, FsmError> {
        let id = req_str(args, 0, "scholar.integrity", "doiOrId")?;
        let rep = self.services.scholar.integrity(&id, cx).await?;
        Ok(HostValue::new(to_json(rep)?))
    }

    async fn scholar_cite(&self, args: &[Value], cx: &CallCx) -> Result<HostValue, FsmError> {
        let input: CiteInput = req_typed(args, 0, "scholar.cite", "idOrWork")?;
        let style: CiteStyle = match args.get(1) {
            None | Some(Value::Null) => CiteStyle::Bibtex,
            Some(v) => serde_json::from_value(v.clone()).map_err(|_| {
                FsmError::invalid(
                    "scholar.cite: style must be one of bibtex, csl, apa, ieee, chicago, vancouver",
                )
            })?,
        };
        let out = self.services.scholar.cite(input, style, cx).await?;
        Ok(HostValue::new(Value::String(out)))
    }

    async fn scholar_entity(
        &self,
        kind: EntityKind,
        args: &[Value],
        cx: &CallCx,
    ) -> Result<HostValue, FsmError> {
        let name = match kind {
            EntityKind::Author => "scholar.author",
            EntityKind::Org => "scholar.org",
            EntityKind::Venue => "scholar.venue",
            EntityKind::Grants => "scholar.grants",
            _ => "scholar.entity",
        };
        let query = req_str(args, 0, name, "query")?;
        let opts = match args.get(1) {
            None | Some(Value::Null) => Value::Object(Map::new()),
            Some(v @ Value::Object(_)) => v.clone(),
            Some(_) => return Err(FsmError::invalid(format!("{name}: opts must be an object"))),
        };
        let val = self.services.scholar.entity(kind, &query, opts, cx).await?;
        Ok(HostValue::new(val))
    }

    async fn doc_read(&self, args: &[Value], cx: &CallCx) -> Result<HostValue, FsmError> {
        let source = req_str(args, 0, "doc.read", "source")?;
        let (start, length) = match args.get(1) {
            None | Some(Value::Null) => (0usize, None),
            Some(Value::Object(obj)) => {
                let start = match obj.get("start").filter(|v| !v.is_null()) {
                    None => 0usize,
                    Some(v) => parse_non_negative_usize(v, "start")?,
                };
                let length = match obj.get("length").filter(|v| !v.is_null()) {
                    None => None,
                    Some(v) => Some(parse_non_negative_usize(v, "length")?),
                };
                (start, length)
            }
            Some(_) => {
                return Err(FsmError::invalid(
                    "doc.read: opts must be an object `{start?, length?}`",
                ));
            }
        };
        let slice = self.services.doc.read(&source, start, length, cx).await?;
        Ok(HostValue::tagged(to_json(slice)?, RenderKind::DocSlice))
    }

    async fn doc_outline(&self, args: &[Value], cx: &CallCx) -> Result<HostValue, FsmError> {
        let source = req_str(args, 0, "doc.outline", "source")?;
        let outline = self.services.doc.outline(&source, cx).await?;
        Ok(HostValue::new(to_json(outline)?))
    }

    async fn source_call(&self, args: &[Value], cx: &CallCx) -> Result<HostValue, FsmError> {
        let op_id = req_str(args, 0, "source.call", "opId")?;
        let params = match args.get(1) {
            None | Some(Value::Null) => Value::Object(Map::new()),
            Some(v @ Value::Object(_)) => v.clone(),
            Some(_) => {
                return Err(FsmError::invalid("source.call: params must be an object"));
            }
        };
        let out = self.services.sources.call(&op_id, params, cx).await?;
        Ok(HostValue::new(to_json(out)?))
    }

    async fn source_protocol(&self, args: &[Value], cx: &CallCx) -> Result<HostValue, FsmError> {
        let kind = req_str(args, 0, "source.protocol", "kind")?;
        let base_url = req_str(args, 1, "source.protocol", "baseUrl")?;
        let op = req_str(args, 2, "source.protocol", "op")?;
        let params = match args.get(3) {
            None | Some(Value::Null) => Value::Object(Map::new()),
            Some(v @ Value::Object(_)) => v.clone(),
            Some(_) => {
                return Err(FsmError::invalid(
                    "source.protocol: params must be an object",
                ));
            }
        };
        let out = self
            .services
            .sources
            .protocol(&kind, &base_url, &op, params, cx)
            .await?;
        Ok(HostValue::new(to_json(out)?))
    }

    async fn cache_search(&self, args: &[Value]) -> Result<HostValue, FsmError> {
        let query = req_str(args, 0, "cache.search", "ftsQuery")?;
        let limit = match args.get(1) {
            None | Some(Value::Null) => 10u32,
            Some(v) => {
                let n = v
                    .as_i64()
                    .ok_or_else(|| FsmError::invalid("cache.search: limit must be an integer"))?;
                n.clamp(1, 50) as u32
            }
        };
        let hits = self.services.store.cache_search(&query, limit).await?;
        Ok(HostValue::new(to_json(hits)?))
    }

    async fn cache_get_page(&self, args: &[Value]) -> Result<HostValue, FsmError> {
        let url = req_str(args, 0, "cache.getPage", "url")?;
        // SPEC §7.2: SSRF layers 1–3 MUST run offline before returning any cached page.
        fsm_net::check_offline(&url, self.services.cfg.allow_private_hosts)?;
        match self.services.store.get_page(&url, None).await? {
            Some(page) => Ok(HostValue::tagged(to_json(page)?, RenderKind::PageResult)),
            None => Ok(HostValue::new(Value::Null)),
        }
    }

    async fn cache_put(&self, args: &[Value], cx: &CallCx) -> Result<HostValue, FsmError> {
        let key = req_str(args, 0, "cache.put", "key")?;
        validate_user_kv_key(&key)?;
        let value = args.get(1).cloned().unwrap_or(Value::Null);

        let mut ttl_s = EXEC_KV_DEFAULT_TTL_S;
        let mut shared = false;
        match args.get(2) {
            None | Some(Value::Null) => {}
            Some(Value::Object(obj)) => {
                if let Some(h) = obj.get("ttlHours").filter(|v| !v.is_null()) {
                    let hours = h.as_f64().ok_or_else(|| {
                        FsmError::invalid("cache.put: opts.ttlHours must be a number")
                    })?;
                    if !hours.is_finite() || hours <= 0.0 {
                        return Err(FsmError::invalid("cache.put: opts.ttlHours must be > 0"));
                    }
                    let secs = (hours * 3600.0).round() as u64;
                    ttl_s = secs.clamp(1, EXEC_KV_MAX_TTL_S);
                }
                if let Some(s) = obj.get("shared").filter(|v| !v.is_null()) {
                    shared = s.as_bool().ok_or_else(|| {
                        FsmError::invalid("cache.put: opts.shared must be a boolean")
                    })?;
                }
            }
            Some(_) => {
                return Err(FsmError::invalid(
                    "cache.put: opts must be an object `{ttlHours?, shared?}`",
                ));
            }
        }

        let session = if shared {
            EXEC_KV_SHARED_SESSION
        } else {
            cx.session_id.as_str()
        };
        self.services
            .store
            .kv_put(session, &key, &value, ttl_s)
            .await?;
        Ok(HostValue::new(Value::Null))
    }

    async fn cache_get(&self, args: &[Value], cx: &CallCx) -> Result<HostValue, FsmError> {
        let key = req_str(args, 0, "cache.get", "key")?;
        let val = self
            .services
            .store
            .kv_get(&cx.session_id, &key)
            .await?
            .unwrap_or(Value::Null);
        Ok(HostValue::new(val))
    }

    async fn catalog_search(&self, args: &[Value]) -> Result<HostValue, FsmError> {
        let query = match args.first() {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => s.clone(),
            Some(_) => {
                return Err(FsmError::invalid("catalog.search: query must be a string"));
            }
        };
        let mut cq: CatalogQuery = opt_struct(args, 1, "catalog.search", "opts")?;
        cq.query = query;
        let hits = self.services.sources.catalog_search(cq).await?;
        Ok(HostValue::tagged(to_json(hits)?, RenderKind::CatalogList))
    }

    async fn catalog_describe(&self, args: &[Value]) -> Result<HostValue, FsmError> {
        let id = req_str(args, 0, "catalog.describe", "id")?;
        let detail = self.services.sources.catalog_describe(&id).await?;
        Ok(HostValue::new(to_json(detail)?))
    }

    async fn util_sleep(&self, args: &[Value], cx: &CallCx) -> Result<HostValue, FsmError> {
        let ms = args
            .first()
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite() && *v > 0.0)
            .unwrap_or(0.0);
        let remaining = cx.remaining();
        let wait = Duration::from_millis(ms as u64).min(remaining);
        tokio::select! {
            biased;
            () = cx.cancel.cancelled() => return Err(FsmError::Timeout(0)),
            () = tokio::time::sleep(wait) => {}
        }
        Ok(HostValue::new(Value::Null))
    }

    async fn util_html_to_md(&self, args: &[Value]) -> Result<HostValue, FsmError> {
        let html = req_str(args, 0, "util.htmlToMd", "html")?;
        let md = self.services.fetch.html_to_md(&html).await?;
        Ok(HostValue::new(Value::String(md)))
    }
}

#[async_trait]
impl HostServices for DefaultHostServices {
    async fn call(&self, func: &str, args: Value, cx: &CallCx) -> Result<HostValue, FsmError> {
        match AssertUnwindSafe(self.dispatch(func, args, cx))
            .catch_unwind()
            .await
        {
            Ok(result) => result,
            Err(payload) => {
                let detail = panic_detail(payload);
                tracing::error!(func, panic = %detail, "host function panicked");
                Err(FsmError::internal(
                    "host_panic",
                    format!("{func} panicked unexpectedly"),
                ))
            }
        }
    }
}

/// Centralized M6 egress taint enforcement (`SPEC.md` §3.4).
async fn check_tainted_egress(func: &str, args: &Value, cx: &CallCx) -> Result<(), FsmError> {
    let first = match args {
        Value::Array(a) => a.first(),
        other => Some(other),
    };
    match func {
        // Local document reads/outlines stay allowed after taint; remote URLs are egress.
        "doc.read" | "doc.outline" => {
            if let Some(Value::String(src)) = first
                && !looks_like_remote_url(src)
            {
                return Ok(());
            }
            Err(FsmError::PermissionDenied(format!(
                "{func} on a remote URL is blocked after reading a local document in this exec (exfiltration guard)"
            )))
        }
        // Narrow M6 exception: GET web.fetch to a host already fetched prior to taint.
        "web.fetch" => {
            if let Some(Value::String(url)) = first {
                let host = extract_host(url);
                if !host.is_empty() && cx.fetched_hosts.lock().await.contains(&host) {
                    return Ok(());
                }
            }
            Err(FsmError::PermissionDenied(
                "web.fetch to a new host is blocked after reading a local document in this exec; only previously fetched hosts are allowed".to_owned(),
            ))
        }
        // Narrow M6 exception: web.fetchBatch only when every URL targets a host already fetched.
        "web.fetchBatch" => {
            if let Some(Value::Array(urls)) = first {
                let fetched = cx.fetched_hosts.lock().await;
                let all_allowed = urls.iter().all(|u| {
                    u.as_str()
                        .map(extract_host)
                        .is_some_and(|h| !h.is_empty() && fetched.contains(&h))
                });
                if all_allowed {
                    return Ok(());
                }
            }
            Err(FsmError::PermissionDenied(
                "web.fetchBatch to a new host is blocked after reading a local document in this exec; only previously fetched hosts are allowed".to_owned(),
            ))
        }
        // Every other `egress = true` function (`web.search`, `web.request`, `web.structured`,
        // `web.research`, `web.compare`, `web.download`, `scholar.*`, `source.call`,
        // `source.protocol`) is unconditionally refused once tainted.
        _ => Err(FsmError::PermissionDenied(format!(
            "{func} is blocked after reading a local document in this exec (exfiltration guard)"
        ))),
    }
}

fn looks_like_remote_url(source: &str) -> bool {
    let s = source.trim().to_ascii_lowercase();
    s.starts_with("http://") || s.starts_with("https://")
}

fn extract_host(url: &str) -> String {
    url::Url::parse(url.trim())
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
        })
        .unwrap_or_default()
}

fn req_str(args: &[Value], idx: usize, func: &str, param: &str) -> Result<String, FsmError> {
    match args.get(idx) {
        Some(Value::String(s)) => Ok(s.clone()),
        None | Some(Value::Null) => Err(FsmError::invalid(format!(
            "{func}: missing required string argument `{param}`"
        ))),
        Some(_) => Err(FsmError::invalid(format!(
            "{func}: `{param}` must be a string"
        ))),
    }
}

fn req_typed<T: DeserializeOwned>(
    args: &[Value],
    idx: usize,
    func: &str,
    param: &str,
) -> Result<T, FsmError> {
    let v = args
        .get(idx)
        .ok_or_else(|| FsmError::invalid(format!("{func}: missing required argument `{param}`")))?;
    serde_json::from_value(v.clone())
        .map_err(|e| FsmError::invalid(format!("{func}: invalid `{param}`: {e}")))
}

fn opt_struct<T: DeserializeOwned + Default>(
    args: &[Value],
    idx: usize,
    func: &str,
    param: &str,
) -> Result<T, FsmError> {
    match args.get(idx) {
        None | Some(Value::Null) => Ok(T::default()),
        Some(v @ Value::Object(_)) => serde_json::from_value(v.clone())
            .map_err(|e| FsmError::invalid(format!("{func}: invalid `{param}`: {e}"))),
        Some(_) => Err(FsmError::invalid(format!(
            "{func}: `{param}` must be an object"
        ))),
    }
}

fn parse_non_negative_usize(v: &Value, field: &str) -> Result<usize, FsmError> {
    if let Some(n) = v.as_i64() {
        if n < 0 {
            return Err(FsmError::invalid(format!("{field} must be >= 0")));
        }
        return usize::try_from(n)
            .map_err(|_| FsmError::invalid(format!("{field} is out of range")));
    }
    if let Some(u) = v.as_u64() {
        return usize::try_from(u)
            .map_err(|_| FsmError::invalid(format!("{field} is out of range")));
    }
    if let Some(f) = v.as_f64() {
        if !f.is_finite() || f < 0.0 {
            return Err(FsmError::invalid(format!("{field} must be >= 0")));
        }
        if f.fract() != 0.0 {
            return Err(FsmError::invalid(format!("{field} must be an integer")));
        }
        return Ok(f as usize);
    }
    Err(FsmError::invalid(format!("{field} must be >= 0")))
}

fn to_json<T: serde::Serialize>(value: T) -> Result<Value, FsmError> {
    serde_json::to_value(value).map_err(|e| FsmError::internal("host_json", e.to_string()))
}

pub(crate) fn panic_detail(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}
