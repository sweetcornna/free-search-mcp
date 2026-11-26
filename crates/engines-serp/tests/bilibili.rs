//! Port of `tests/test_bilibili.py`: `all/v2` URL building, `buvid3` header,
//! video-only group extraction, `<em>` stripping, protocol-relative URL upgrade,
//! unix `pubdate` formatting, and `-412` / garbage handling.

use async_trait::async_trait;
use fsm_core::config::Config;
use fsm_core::net::{HttpBytes, HttpRequest, NetClient, NetCx};
use fsm_core::source::{Source, SrcCx, SrcOutput, TransportProfile};
use fsm_core::types::HitKind;
use fsm_core::{FsmError, NullStore, SourceDeps};
use fsm_engines_serp::common::SerpQuery;
use fsm_engines_serp::engines::bilibili::{BilibiliEngine, build_url, parse, synthetic_buvid3};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn sample_json() -> String {
    json!({
        "code": 0,
        "data": {
            "result": [
                {
                    "result_type": "bili_user",
                    "data": [{"uname": "Some Creator", "mid": 12345}]
                },
                {
                    "result_type": "video",
                    "data": [
                        {
                            "title": r#"Learn <em class="keyword">Python</em> fast"#,
                            "arcurl": "//www.bilibili.com/video/BV1aa",
                            "author": "TeacherA",
                            "description": "A beginner Python tutorial.",
                            "pubdate": 1714316400,
                            "play": 99999
                        },
                        {
                            "title": "Plain title no tags",
                            "arcurl": "http://www.bilibili.com/video/BV1bb",
                            "author": "TeacherB",
                            "description": "Second video.",
                            "pubdate": 0,
                            "play": 10
                        }
                    ]
                }
            ]
        }
    })
    .to_string()
}

struct MockBiliNet {
    status: u16,
    body: String,
    seen_headers: Mutex<Vec<(String, String)>>,
    seen_redacted: Mutex<Vec<String>>,
}

impl MockBiliNet {
    fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            seen_headers: Mutex::new(Vec::new()),
            seen_redacted: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl NetClient for MockBiliNet {
    async fn send(
        &self,
        req: HttpRequest,
        _profile: TransportProfile,
        _cx: &NetCx,
    ) -> Result<HttpBytes, FsmError> {
        *self.seen_headers.lock().unwrap() = req.headers.clone();
        *self.seen_redacted.lock().unwrap() = req.redact_headers.clone();
        Ok(HttpBytes::new(
            req.url,
            self.status,
            vec![],
            self.body.clone(),
        ))
    }

    fn check_url_offline(&self, _url: &str) -> Result<(), FsmError> {
        Ok(())
    }
}

fn default_cx() -> SrcCx {
    SrcCx::from_config(
        &Config::default(),
        CancellationToken::new(),
        Duration::from_secs(5),
    )
}

#[test]
fn test_build_url_hits_all_v2_endpoint_and_buvid3_shape() {
    let q = SerpQuery::from_call(
        "bilibili",
        "search",
        &json!({"query": "hello world", "limit": 10}),
        &default_cx(),
        &Config::default(),
    )
    .unwrap();
    let url = build_url(&q);
    assert!(url.starts_with("https://api.bilibili.com/x/web-interface/search/all/v2?"));
    assert!(url.contains("keyword=hello+world"));
    assert!(url.contains("page=1"));

    let b3 = synthetic_buvid3();
    assert_eq!(b3.len(), 37);
    assert!(b3.ends_with("infoc"));
    assert!(
        b3[..32]
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase())
    );
}

#[test]
fn test_parse_extracts_video_group_only() {
    let out = parse(&sample_json());
    assert_eq!(out.len(), 2);

    let first = &out[0];
    assert_eq!(first.title, "Learn Python fast");
    assert!(!first.title.contains("<em") && !first.title.contains("</em>"));
    assert_eq!(first.url, "https://www.bilibili.com/video/BV1aa");
    assert_eq!(first.snippet, "A beginner Python tutorial.");
    assert_eq!(first.date.as_deref(), Some("2024-04-28"));
    assert_eq!(first.date_source.as_deref(), Some("api"));
    assert_eq!(first.kind, HitKind::Vertical);

    let second = &out[1];
    assert_eq!(second.title, "Plain title no tags");
    // Plain http:// arcurl is upgraded to https://
    assert_eq!(second.url, "https://www.bilibili.com/video/BV1bb");
    assert_eq!(second.date, None);
}

#[test]
fn test_parse_returns_empty_on_bad_or_empty_input() {
    for raw in [
        "",
        "garbage",
        r#"{"code":-412}"#,
        r#"{"code":0,"data":{}}"#,
        r#"{"code":0,"data":{"result":[]}}"#,
        "[1, 2, 3]",
    ] {
        assert!(parse(raw).is_empty(), "raw={raw}");
    }
}

#[tokio::test]
async fn test_search_sends_buvid3_referer_and_handles_truncation_and_errors() {
    let net = Arc::new(MockBiliNet::new(200, sample_json()));
    let deps = SourceDeps::new(
        net.clone(),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let engine = BilibiliEngine::new(deps);

    let SrcOutput::Hits(out) = engine
        .call(
            "search",
            json!({"query": "python", "limit": 10}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert_eq!(out.len(), 2);
    assert!(out.iter().all(|h| h.source == "bilibili"));
    let headers = net.seen_headers.lock().unwrap().clone();
    assert!(headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("cookie")
        && v.starts_with("buvid3=")
        && v.ends_with("infoc")));
    assert!(
        headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("referer") && v == "https://www.bilibili.com")
    );
    assert!(
        net.seen_redacted
            .lock()
            .unwrap()
            .iter()
            .any(|h| h.eq_ignore_ascii_case("cookie"))
    );

    // Truncation
    let SrcOutput::Hits(one) = engine
        .call(
            "search",
            json!({"query": "python", "limit": 1}),
            &default_cx(),
        )
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert_eq!(one.len(), 1);

    // Empty / garbage JSON with HTTP 200 -> empty hits
    for bad in ["", "garbage"] {
        let d = SourceDeps::new(
            Arc::new(MockBiliNet::new(200, bad)),
            Arc::new(NullStore::new()),
            Arc::new(Config::default()),
        );
        let SrcOutput::Hits(h) = BilibiliEngine::new(d)
            .call(
                "search",
                json!({"query": "python", "limit": 10}),
                &default_cx(),
            )
            .await
            .unwrap()
        else {
            panic!("expected hits");
        };
        assert!(h.is_empty(), "bad={bad}");
    }

    // M9: code -412 ("request intercepted") surfaces as Upstream wall error.
    let d_412 = SourceDeps::new(
        Arc::new(MockBiliNet::new(
            200,
            r#"{"code":-412,"message":"blocked"}"#,
        )),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let err_412 = BilibiliEngine::new(d_412)
        .call(
            "search",
            json!({"query": "python", "limit": 10}),
            &default_cx(),
        )
        .await
        .unwrap_err();
    match err_412 {
        FsmError::Upstream {
            source_id, message, ..
        } => {
            assert_eq!(source_id, "bilibili");
            assert!(
                message.contains("intercepted-gated"),
                "unexpected -412 message: {message}"
            );
        }
        other => panic!("expected Upstream for -412, got {other:?}"),
    }

    // M9: non-200 HTTP status surfaces as Upstream error (429 -> RateLimited).
    let d_503 = SourceDeps::new(
        Arc::new(MockBiliNet::new(503, "Service Unavailable")),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let err_503 = BilibiliEngine::new(d_503)
        .call(
            "search",
            json!({"query": "python", "limit": 10}),
            &default_cx(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err_503,
        FsmError::Upstream {
            status: Some(503),
            ..
        }
    ));
}

#[tokio::test]
async fn test_search_applies_post_filters() {
    // Port of `test_bilibili.py::test_search_applies_post_filters` adapted for
    // 1.0's domain/filetype post-filters (exclude_domains + include_domains).
    let mixed = json!({
        "code": 0,
        "data": {
            "result": [{
                "result_type": "video",
                "data": [
                    {
                        "title": "Video on bilibili.com",
                        "arcurl": "https://www.bilibili.com/video/BV1aa",
                        "description": "First video",
                        "pubdate": 1714316400
                    },
                    {
                        "title": "Video on other.example",
                        "arcurl": "https://other.example/video/2",
                        "description": "Second video",
                        "pubdate": 1714316400
                    }
                ]
            }]
        }
    })
    .to_string();
    let deps = SourceDeps::new(
        Arc::new(MockBiliNet::new(200, mixed)),
        Arc::new(NullStore::new()),
        Arc::new(Config::default()),
    );
    let engine = BilibiliEngine::new(deps);
    let mut cx = default_cx();
    cx.domains = vec!["-other.example".into()];
    let SrcOutput::Hits(out) = engine
        .call("search", json!({"query": "python", "limit": 10}), &cx)
        .await
        .unwrap()
    else {
        panic!("expected hits");
    };
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].url, "https://www.bilibili.com/video/BV1aa");
}

#[test]
fn test_parse_decodes_full_html5_entities_in_titles_and_snippets() {
    // M7: full HTML5 entities (not just 22 hardcoded ones) are decoded.
    let body = json!({
        "code": 0,
        "data": {
            "result": [{
                "result_type": "video",
                "data": [{
                    "title": "Caf&eacute; &uuml;ber &ccedil;a",
                    "arcurl": "https://www.bilibili.com/video/BV1ent",
                    "description": "Price: &euro;10 &mdash; na&iuml;ve",
                    "pubdate": 1714316400
                }]
            }]
        }
    })
    .to_string();
    let out = parse(&body);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].title, "Café über ça");
    assert_eq!(out[0].snippet, "Price: €10 — naïve");
}
