//! Bilibili keyless video-search JSON engine (`api.bilibili.com/x/web-interface/search/all/v2`).
//!
//! Port of `src/search_mcp/engines/bilibili.py`:
//! - Mints a fresh synthetic `buvid3=<32 uppercase hex>infoc` cookie per request
//!   (marked redacted so fixtures never depend on random cookies) + `Referer: https://www.bilibili.com`;
//! - Extracts only `result_type == "video"` items;
//! - Strips `<em class="keyword">` highlight HTML and upgrades `//` & `http://`
//!   `arcurl` links to `https://`;
//! - Formats unix-seconds `pubdate` as UTC `YYYY-MM-DD` (`date_source = "api"`).

use crate::common::{
    SerpQuery, check_http_status, decode_body, finalize_hits, make_hit, net_cx_for, quote_plus,
    search_op_decl, strip_html_tags, unix_seconds_to_iso_date,
};
use async_trait::async_trait;
use fsm_core::net::HttpRequest;
use fsm_core::source::{
    AuthMode, Capability, PoolRole, Source, SourceDecl, SourceLimits, SrcCx, SrcOutput,
    TransportProfile,
};
use fsm_core::types::{Hit, HitKind};
use fsm_core::{FsmError, SourceDeps};
use rand::Rng as _;
use serde_json::Value;

pub const ENDPOINT: &str = "https://api.bilibili.com/x/web-interface/search/all/v2";

/// Generate a synthetic `buvid3` cookie (`<32 uppercase hex>infoc`).
pub fn synthetic_buvid3() -> String {
    let mut rng = rand::rng();
    let hi: u64 = rng.next_u64();
    let lo: u64 = rng.next_u64();
    format!("{hi:016X}{lo:016X}infoc")
}

/// Build the Bilibili `all/v2` search URL.
pub fn build_url(q: &SerpQuery) -> String {
    format!("{ENDPOINT}?keyword={}&page=1", quote_plus(&q.query))
}

fn format_pubdate(val: Option<&Value>) -> Option<String> {
    let v = val?;
    let ts = if let Some(i) = v.as_i64() {
        i
    } else {
        v.as_str()?.trim().parse::<i64>().ok()?
    };
    unix_seconds_to_iso_date(ts)
}

/// Parse Bilibili `all/v2` JSON response into video `Hit`s.
pub fn parse(body: &str) -> Vec<Hit> {
    if body.is_empty() {
        return Vec::new();
    }
    let Ok(Value::Object(payload)) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    if payload.get("code").and_then(Value::as_i64) != Some(0) {
        return Vec::new();
    }
    let Some(groups) = payload
        .get("data")
        .and_then(Value::as_object)
        .and_then(|d| d.get("result"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    let mut results = Vec::new();
    for group in groups {
        let Some(gobj) = group.as_object() else {
            continue;
        };
        if gobj.get("result_type").and_then(Value::as_str) != Some("video") {
            continue;
        }
        let Some(items) = gobj.get("data").and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let title = strip_html_tags(obj.get("title").and_then(Value::as_str).unwrap_or(""));
            let raw_url = obj
                .get("arcurl")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            let url = if raw_url.starts_with("//") {
                format!("https:{raw_url}")
            } else if let Some(rest) = raw_url.strip_prefix("http://") {
                format!("https://{rest}")
            } else {
                raw_url.to_owned()
            };
            if title.is_empty() || url.is_empty() {
                continue;
            }
            let snippet =
                strip_html_tags(obj.get("description").and_then(Value::as_str).unwrap_or(""));
            let date = format_pubdate(obj.get("pubdate"));
            results.push(make_hit(
                "bilibili",
                HitKind::Vertical,
                title,
                url,
                snippet,
                date,
                Some("api"),
            ));
        }
    }
    results
}

pub struct BilibiliEngine {
    decl: SourceDecl,
    deps: SourceDeps,
}

impl BilibiliEngine {
    pub fn new(deps: SourceDeps) -> Self {
        let mut decl = SourceDecl::new(
            "bilibili",
            "Bilibili",
            "哔哩哔哩: Chinese video search with uploader, duration and view counts.",
        )
        .with_scopes(&["media"])
        .with_caps(&[Capability::Search])
        .with_op(search_op_decl(
            "Search videos on Bilibili.",
            "python asyncio",
            "bilibili",
        ));
        decl.aliases = vec!["b站".into(), "哔哩哔哩".into()];
        decl.home_url = Some("https://www.bilibili.com".into());
        decl.tier = 2;
        decl.auth = AuthMode::None;
        decl.transport = TransportProfile::Chrome;
        decl.pool_role = PoolRole::Explicit;
        decl.limits = SourceLimits::new(1.0, 3, 2, 5_000, 21_600);
        Self { decl, deps }
    }
}

#[async_trait]
impl Source for BilibiliEngine {
    fn decl(&self) -> &SourceDecl {
        &self.decl
    }

    async fn call(&self, op: &str, params: Value, cx: &SrcCx) -> Result<SrcOutput, FsmError> {
        let q = SerpQuery::from_call(&self.decl.id, op, &params, cx, &self.deps.cfg)?;
        let url = build_url(&q);
        let ncx = net_cx_for(cx, &q, &self.decl.id, self.decl.limits.rps);
        let buvid3 = synthetic_buvid3();
        let req = HttpRequest::get(&url)
            .header("Cookie", format!("buvid3={buvid3}"))
            .header("Referer", "https://www.bilibili.com")
            .header("Accept", "application/json")
            .header("Accept-Language", &q.accept_language)
            .redact_header("cookie")
            .for_source(&self.decl.id);

        let resp = self.deps.net.send(req, self.decl.transport, &ncx).await?;
        check_http_status(&self.decl.id, &resp)?;
        let body = decode_body(&resp);
        if let Ok(Value::Object(payload)) = serde_json::from_str::<Value>(&body)
            && payload.get("code").and_then(Value::as_i64) == Some(-412)
        {
            return Err(FsmError::upstream(
                &self.decl.id,
                Some(resp.status),
                "bilibili was intercepted-gated",
            ));
        }
        let parsed = parse(&body);
        let hits = finalize_hits(
            parsed,
            &q.include_domains,
            &q.exclude_domains,
            q.filetype.as_deref(),
            q.limit,
        );
        Ok(SrcOutput::Hits(hits))
    }
}
