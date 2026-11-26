//! Tests for `web.request` (`FetchApi::request`): N1 contract, all 5 hard guardrails,
//! and NVD replay equality assertion (`REDTEAM-2.md` N1).

mod common;

use common::{MemStore, MockDocs, MockNet, MockSearch, make_service};
use encoding_rs::GBK;
use fsm_core::config::{Config, RawHttpPolicy};
use fsm_core::fixture::ReplayNet;
use fsm_core::{
    CallCx, FetchApi, FsmError, HttpBytes, HttpMethod, RawRequestOpts, TransportProfile,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// N1 mandatory replay assertion: using the real NVD JSON fixture, assert that
/// `JSON.parse((await web.request({url})).body)` is deeply equal to `.json`,
/// and that wire JSON exposes `body` + `json` with NO `text` field.
#[tokio::test]
async fn test_request_nvd_replay_body_and_json_deep_equality() {
    let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../manifests/fixtures/w5c/nvd/cve/default.json");
    let net = Arc::new(ReplayNet::from_file(&fixture_path).expect("load NVD fixture"));

    let svc = make_service(
        net.clone(),
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );

    let mut headers = BTreeMap::new();
    headers.insert("Accept".into(), "application/json".into());
    let mut opts =
        RawRequestOpts::new("https://services.nvd.nist.gov/rest/json/cves/2.0?cveId=CVE-2024-3094");
    opts.headers = Some(headers);

    let resp = svc.request(opts, &CallCx::default()).await.unwrap();
    assert_eq!(resp.status, 200);
    assert!(!resp.truncated);

    // Core N1 equality assertion: JSON.parse(resp.body) == resp.json
    let parsed_from_body: Value = serde_json::from_str(&resp.body).expect("body is valid JSON");
    assert_eq!(
        resp.json.as_ref(),
        Some(&parsed_from_body),
        "resp.json must deeply equal JSON.parse(resp.body)"
    );
    assert_eq!(
        parsed_from_body["vulnerabilities"][0]["cve"]["id"],
        "CVE-2024-3094"
    );

    // Wire JSON shape: has "body" and "json", NEVER "text".
    let wire = serde_json::to_value(&resp).unwrap();
    assert!(wire.get("body").is_some());
    assert!(wire.get("json").is_some());
    assert!(
        wire.get("text").is_none(),
        "N1 forbids .text on HttpResponse"
    );
    assert!(net.unmatched().is_empty() && net.hits().len() == 1);
}

#[tokio::test]
async fn test_guardrail_1_plain_client_ssrf_and_redirect_cap() {
    // Blocked initial URL never hits net.
    let net = Arc::new(MockNet::new());
    let svc = make_service(
        net.clone(),
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    assert!(matches!(
        svc.request(
            RawRequestOpts::new("http://169.254.169.254/"),
            &CallCx::default()
        )
        .await,
        Err(FsmError::UnsafeUrl(_))
    ));
    assert!(net.recorded().is_empty());

    // Redirect to private IP blocked on hop.
    let net_redir = Arc::new(MockNet::with_responses(vec![Ok(HttpBytes::new(
        "https://api.example/start",
        302,
        vec![("location".into(), "http://127.0.0.1/internal".into())],
        "",
    ))]));
    let svc2 = make_service(
        net_redir.clone(),
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    assert!(matches!(
        svc2.request(
            RawRequestOpts::new("https://api.example/start"),
            &CallCx::default()
        )
        .await,
        Err(FsmError::UnsafeUrl(_))
    ));
    // Must use Tier-0 Plain client and no cookie jar.
    let (req, profile) = &net_redir.recorded()[0];
    assert_eq!(*profile, TransportProfile::Plain);
    assert!(req.cookie_jar_id.is_none());
    assert!(!req.follow_redirects);

    // > 5 redirects rejected.
    let hops = (0..8)
        .map(|i| {
            Ok(HttpBytes::new(
                format!("https://api.example/{i}"),
                302,
                vec![("location".into(), format!("https://api.example/{}", i + 1))],
                "",
            ))
        })
        .collect();
    let net_loop = Arc::new(MockNet::with_responses(hops));
    let svc3 = make_service(
        net_loop.clone(),
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let err = svc3
        .request(
            RawRequestOpts::new("https://api.example/0"),
            &CallCx::default(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("too many redirects"));
    assert_eq!(net_loop.recorded().len(), 6);
}

#[tokio::test]
async fn test_guardrail_2_kill_switch_and_post_rules() {
    // Policy = Off blocks GET too.
    let mut cfg_off = Config::default();
    cfg_off.exec_raw_http = RawHttpPolicy::Off;
    let svc_off = make_service(
        Arc::new(MockNet::new()),
        Arc::new(MemStore::new()),
        cfg_off,
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    assert!(matches!(
        svc_off
            .request(
                RawRequestOpts::new("https://api.example/x"),
                &CallCx::default()
            )
            .await,
        Err(FsmError::PermissionDenied(_))
    ));

    // Policy = GetOnly allows GET/HEAD, blocks POST.
    let mut cfg_go = Config::default();
    cfg_go.exec_raw_http = RawHttpPolicy::GetOnly;
    let net_go = Arc::new(MockNet::with_responses(vec![Ok(HttpBytes::new(
        "https://api.example/x",
        200,
        vec![("content-type".into(), "text/plain".into())],
        "ok",
    ))]));
    let svc_go = make_service(
        net_go,
        Arc::new(MemStore::new()),
        cfg_go,
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    assert_eq!(
        svc_go
            .request(
                RawRequestOpts::new("https://api.example/x"),
                &CallCx::default()
            )
            .await
            .unwrap()
            .body,
        "ok"
    );
    let mut post_opts = RawRequestOpts::new("https://api.example/x");
    post_opts.method = Some(HttpMethod::Post);
    post_opts.body = Some(json!({"a": 1}));
    assert!(matches!(
        svc_go.request(post_opts, &CallCx::default()).await,
        Err(FsmError::PermissionDenied(_))
    ));

    // Policy = GetPost: rejects multipart/form-data, >128 KiB body, and body on GET.
    let net_gp = Arc::new(MockNet::with_handler(|req, _, _| {
        Ok(HttpBytes::new(
            req.url.clone(),
            200,
            vec![("content-type".into(), "application/json".into())],
            r#"{"ok":true}"#,
        ))
    }));
    let svc_gp = make_service(
        net_gp.clone(),
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );

    // multipart/form-data rejected
    let mut mp = RawRequestOpts::new("https://api.example/upload");
    mp.method = Some(HttpMethod::Post);
    mp.headers = Some(BTreeMap::from([(
        "Content-Type".into(),
        "multipart/form-data; boundary=abc".into(),
    )]));
    mp.body = Some(Value::String("--abc--".into()));
    assert!(matches!(
        svc_gp.request(mp, &CallCx::default()).await,
        Err(FsmError::InvalidInput(_))
    ));

    // > 128 KiB rejected
    let mut huge = RawRequestOpts::new("https://api.example/huge");
    huge.method = Some(HttpMethod::Post);
    huge.body = Some(Value::String("x".repeat(128 * 1024 + 1)));
    assert!(matches!(
        svc_gp.request(huge, &CallCx::default()).await,
        Err(FsmError::InvalidInput(_))
    ));

    // GET with body rejected
    let mut get_body = RawRequestOpts::new("https://api.example/get");
    get_body.body = Some(json!({"nope": true}));
    assert!(matches!(
        svc_gp.request(get_body, &CallCx::default()).await,
        Err(FsmError::InvalidInput(_))
    ));
    assert!(net_gp.recorded().is_empty());

    // Allowed types succeed: json, form-urlencoded, sparql-query, text/plain.
    for (ct, body) in [
        ("application/json", json!({"q": "rust"})),
        (
            "application/x-www-form-urlencoded",
            json!({"a": "1", "b": "two"}),
        ),
        (
            "application/sparql-query",
            Value::String("SELECT * WHERE { ?s ?p ?o }".into()),
        ),
        ("text/plain", Value::String("hello".into())),
    ] {
        let mut o = RawRequestOpts::new("https://api.example/post");
        o.method = Some(HttpMethod::Post);
        o.headers = Some(BTreeMap::from([("Content-Type".into(), ct.into())]));
        o.body = Some(body);
        let r = svc_gp.request(o, &CallCx::default()).await.unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.json, Some(json!({"ok": true})));
    }
}

#[tokio::test]
async fn test_guardrail_3_strips_auth_headers_and_set_cookie() {
    let sensitive = [
        ("Authorization", "Bearer secret"),
        ("Proxy-Authorization", "Basic secret"),
        ("Cookie", "sid=123"),
        ("X-Api-Key", "k1"),
        ("Api-Key", "k2"),
        ("X-Auth-Token", "t1"),
        ("Accept", "application/json"),
    ];
    let mk_headers = || -> BTreeMap<String, String> {
        sensitive
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    };

    // Default EXEC_AUTH_HOSTS is empty: all 6 sensitive headers stripped, Set-Cookie stripped from response.
    let net = Arc::new(MockNet::with_responses(vec![Ok(HttpBytes::new(
        "https://api.example/v1",
        200,
        vec![
            ("Content-Type".into(), "application/json".into()),
            ("Set-Cookie".into(), "session=evil; HttpOnly".into()),
            ("X-RateLimit".into(), "100".into()),
        ],
        r#"{"ok":1}"#,
    ))]));
    let svc = make_service(
        net.clone(),
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let mut opts = RawRequestOpts::new("https://api.example/v1");
    opts.headers = Some(mk_headers());
    let resp = svc.request(opts, &CallCx::default()).await.unwrap();

    let sent_headers = &net.recorded()[0].0.headers;
    assert_eq!(
        sent_headers.len(),
        1,
        "only Accept should survive: {sent_headers:?}"
    );
    assert_eq!(sent_headers[0].0, "Accept");
    assert!(!resp.headers.contains_key("set-cookie"));
    assert_eq!(
        resp.headers.get("x-ratelimit").map(String::as_str),
        Some("100")
    );

    // With EXEC_AUTH_HOSTS = ["trusted.example"], headers pass to trusted.example,
    // AND get stripped if it redirects to untrusted.example!
    let mut cfg = Config::default();
    cfg.exec_auth_hosts = vec!["trusted.example".into()];
    let net2 = Arc::new(MockNet::with_responses(vec![
        Ok(HttpBytes::new(
            "https://api.trusted.example/start",
            302,
            vec![("location".into(), "https://other.example/dest".into())],
            "",
        )),
        Ok(HttpBytes::new(
            "https://other.example/dest",
            200,
            vec![("content-type".into(), "text/plain".into())],
            "done",
        )),
    ]));
    let svc2 = make_service(
        net2.clone(),
        Arc::new(MemStore::new()),
        cfg,
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );
    let mut opts2 = RawRequestOpts::new("https://api.trusted.example/start");
    opts2.headers = Some(mk_headers());
    let r2 = svc2.request(opts2, &CallCx::default()).await.unwrap();
    assert_eq!(r2.body, "done");
    let rec = net2.recorded();
    assert!(
        rec[0]
            .0
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("authorization")),
        "trusted host should receive Authorization"
    );
    assert!(
        !rec[1]
            .0
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("authorization")),
        "cross-host redirect to untrusted host MUST strip Authorization"
    );
}

#[tokio::test]
async fn test_n1_head_charset_binary_and_4mib_cap() {
    let (gbk_bytes, _, _) = GBK.encode("中文响应内容");

    let net = Arc::new(MockNet::with_responses(vec![
        // 1. HEAD -> empty body, json: None
        Ok(HttpBytes::new(
            "https://api.example/head",
            200,
            vec![("content-type".into(), "application/json".into())],
            r#"{"ignored":true}"#,
        )),
        // 2. Non-UTF-8 (GBK) decoded cleanly
        Ok(HttpBytes::new(
            "https://api.example/gbk",
            200,
            vec![("content-type".into(), "text/plain; charset=gbk".into())],
            gbk_bytes.into_owned(),
        )),
        // 3. Binary -> exact InvalidInput error
        Ok(HttpBytes::new(
            "https://api.example/file.pdf",
            200,
            vec![("content-type".into(), "application/pdf".into())],
            b"%PDF-1.7".as_slice(),
        )),
        // 4. Malformed JSON with application/json -> body preserved, json = None
        Ok(HttpBytes::new(
            "https://api.example/badjson",
            200,
            vec![("content-type".into(), "application/json".into())],
            "{not valid json",
        )),
        // 5. +json media type parses into .json
        Ok(HttpBytes::new(
            "https://api.example/problem",
            400,
            vec![("content-type".into(), "application/problem+json".into())],
            r#"{"title":"Bad Request"}"#,
        )),
        // 6. > 4 MiB response gets capped and marked truncated
        Ok(HttpBytes::new(
            "https://api.example/big",
            200,
            vec![("content-type".into(), "text/plain".into())],
            vec![b'a'; 4 * 1024 * 1024 + 512],
        )),
    ]));

    let svc = make_service(
        net.clone(),
        Arc::new(MemStore::new()),
        Config::default(),
        Arc::new(MockDocs::default()),
        Arc::new(MockSearch::default()),
    );

    // 1. HEAD
    let mut head_opts = RawRequestOpts::new("https://api.example/head");
    head_opts.method = Some(HttpMethod::Head);
    let r1 = svc.request(head_opts, &CallCx::default()).await.unwrap();
    assert_eq!(r1.body, "");
    assert!(r1.json.is_none());

    // 2. GBK
    let r2 = svc
        .request(
            RawRequestOpts::new("https://api.example/gbk"),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(r2.body, "中文响应内容");
    assert!(r2.json.is_none());

    // 3. Binary
    let err = svc
        .request(
            RawRequestOpts::new("https://api.example/file.pdf"),
            &CallCx::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, FsmError::InvalidInput(_)));
    assert_eq!(
        err.to_string(),
        "binary response; use web.download or doc.read"
    );

    // 4. Malformed JSON
    let r4 = svc
        .request(
            RawRequestOpts::new("https://api.example/badjson"),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(r4.body, "{not valid json");
    assert!(r4.json.is_none());

    // 5. +json
    let r5 = svc
        .request(
            RawRequestOpts::new("https://api.example/problem"),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert_eq!(r5.status, 400);
    assert_eq!(r5.json, Some(json!({"title": "Bad Request"})));

    // 6. 4 MiB cap (and verify request asked for 4 MiB max_bytes)
    let r6 = svc
        .request(
            RawRequestOpts::new("https://api.example/big"),
            &CallCx::default(),
        )
        .await
        .unwrap();
    assert!(r6.truncated);
    assert_eq!(r6.body.len(), 4 * 1024 * 1024);
    assert_eq!(net.recorded()[5].0.max_bytes, Some(4 * 1024 * 1024));
}
