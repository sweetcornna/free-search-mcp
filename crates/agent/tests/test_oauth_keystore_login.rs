//! Port of `tests/test_oauth.py`, `tests/test_keystore.py`, and `tests/test_login_cli.py`.
//!
//! Offline: token endpoints use `wiremock`, loopback callbacks connect over TCP on free
//! ephemeral ports, and all files stay inside `TempDir`.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use fsm_agent::keystore;
use fsm_agent::login;
use fsm_agent::oauth::{
    Credential, LoginOpts, OAuthManager, OAuthProvider, antigravity_client, apply_codex_claims,
    can_open_browser_for, credential_path, is_signed_in, jwt_claims, link_codex_cli, load,
    now_unix, parse_pasted, pkce_pair, read_redirect, save, sign_out, state_matches, status,
};
use fsm_core::Config;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::net::TcpListener as StdTcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn make_jwt(claims: serde_json::Value) -> String {
    let h = URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
    let p = URL_SAFE_NO_PAD.encode(claims.to_string());
    format!("{h}.{p}.sig")
}

fn free_ports(n: usize) -> Vec<u16> {
    let listeners: Vec<StdTcpListener> = (0..n)
        .map(|_| StdTcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    listeners
        .iter()
        .map(|l| l.local_addr().unwrap().port())
        .collect()
}

async fn http_get(port: u16, target: &str) -> (String, String) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let req = format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let status_line = head.lines().next().unwrap_or("").to_owned();
    (status_line, body.to_owned())
}

// ---------------------------------------------------------------------------
// Keystore tests (test_keystore.py)
// ---------------------------------------------------------------------------

#[test]
fn test_keystore_roundtrip_permissions_hot_reload_and_corrupt() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    keystore::reset_cache();

    assert!(keystore::get_secret("serper_api_key", dir).is_none());

    let mut m = BTreeMap::new();
    m.insert("serper_api_key".into(), "abc123".into());
    m.insert("tavily_api_key".into(), "tvly-1".into());
    keystore::set_secrets(&m, dir).unwrap();
    assert_eq!(
        keystore::get_secret("serper_api_key", dir).as_deref(),
        Some("abc123")
    );
    assert_eq!(
        keystore::get_secret("tavily_api_key", dir).as_deref(),
        Some("tvly-1")
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(keystore::config_path(dir))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    // Partial update & empty deletion
    let mut upd = BTreeMap::new();
    upd.insert("serper_api_key".into(), "".into());
    keystore::set_secrets(&upd, dir).unwrap();
    assert!(keystore::get_secret("serper_api_key", dir).is_none());
    assert_eq!(
        keystore::get_secret("tavily_api_key", dir).as_deref(),
        Some("tvly-1")
    );

    // External out-of-band write picked up via hot-reload (length/mtime changes)
    std::fs::write(
        keystore::config_path(dir),
        r#"{"secrets":{"serper_api_key":"external-live-val-123456"}}"#,
    )
    .unwrap();
    assert_eq!(
        keystore::get_secret("serper_api_key", dir).as_deref(),
        Some("external-live-val-123456")
    );

    // Corrupt file yields empty, not panic
    std::fs::write(keystore::config_path(dir), "{ not json").unwrap();
    assert!(keystore::all_secrets(dir).is_empty());
}

// ---------------------------------------------------------------------------
// OAuth pure pieces & store tests (test_oauth.py)
// ---------------------------------------------------------------------------

#[test]
fn test_pkce_authorize_links_redirect_and_jwt_claims() {
    let (verifier, challenge) = pkce_pair();
    assert!((43..=128).contains(&verifier.len()));
    let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    assert_eq!(challenge, expected);

    let codex = OAuthProvider::codex();
    assert_eq!(codex.redirect_ports, vec![1455, 1457]);
    let link = codex.authorize_link("CHALLENGE", "STATE", &codex.redirect_uri(1455));
    assert!(link.starts_with("https://auth.openai.com/oauth/authorize?"));
    assert!(!link.contains('+'), "spaces must be %20, not +: {link}");
    assert!(link.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
    assert!(link.contains("codex_cli_simplified_flow=true"));
    assert!(link.contains("id_token_add_organizations=true"));

    // State prefix match + rejections
    assert!(state_matches("s1-onboarding", "s1"));
    assert!(!state_matches("other", "s1"));
    assert!(!state_matches("", ""));

    let mut params = BTreeMap::new();
    params.insert("code".into(), vec!["abc".into()]);
    params.insert("state".into(), vec!["s1-suffix".into()]);
    assert_eq!(read_redirect(&params, "s1").unwrap(), "abc");

    let mut ent = BTreeMap::new();
    ent.insert("error".into(), vec!["access_denied".into()]);
    ent.insert(
        "error_description".into(),
        vec!["missing_codex_entitlement".into()],
    );
    ent.insert("state".into(), vec!["s1".into()]);
    assert!(
        read_redirect(&ent, "s1")
            .unwrap_err()
            .to_string()
            .contains("does not include Codex")
    );

    assert_eq!(
        parse_pasted(
            "http://127.0.0.1:1455/auth/callback?code=xyz&state=s1\n",
            "s1"
        )
        .unwrap(),
        "xyz"
    );
    assert_eq!(parse_pasted("?code=xyz&state=s1", "s1").unwrap(), "xyz");
    assert!(parse_pasted("just some text", "s1").is_err());

    assert_eq!(jwt_claims("not-a-jwt"), json!({}));
    assert_eq!(jwt_claims(&make_jwt(json!({"exp": 5}))), json!({"exp": 5}));

    let mut cred = Credential {
        provider: "codex".into(),
        ..Credential::default()
    };
    let id_tok = make_jwt(json!({
        "https://api.openai.com/profile": {"email": "p@example.com"},
        "organizations": [{"id": "org-first"}]
    }));
    apply_codex_claims(&mut cred, &id_tok);
    assert_eq!(cred.email, "p@example.com");
    assert_eq!(cred.account_id, "org-first");
}

#[test]
fn test_store_permissions_no_token_in_status_and_codex_cli_link() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    assert!(load(dir, "codex", None).is_none());
    assert!(!status(dir, "codex", None).unwrap().signed_in);
    assert!(
        OAuthProvider::by_id("gemini-cli")
            .unwrap_err()
            .to_string()
            .contains("codex, antigravity")
    );

    let cred = Credential {
        provider: "codex".into(),
        access_token: "ACCESS-SECRET".into(),
        refresh_token: "REFRESH-SECRET".into(),
        expires_at: now_unix() + 3600.0,
        account_id: "acct-1".into(),
        email: "me@example.com".into(),
        plan: "plus".into(),
        source: "oauth".into(),
        ..Credential::default()
    };
    save(dir, &cred).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = credential_path(dir, "codex");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    let st_json = serde_json::to_string(&status(dir, "codex", None).unwrap()).unwrap();
    assert!(!st_json.contains("ACCESS-SECRET"));
    assert!(!st_json.contains("REFRESH-SECRET"));
    assert!(st_json.contains("me@example.com") && st_json.contains("plus"));

    assert!(sign_out(dir, "codex"));
    assert!(!sign_out(dir, "codex"));

    // Corrupt file reads as signed out
    let p = credential_path(dir, "codex");
    std::fs::write(&p, "{corrupt").unwrap();
    assert!(load(dir, "codex", None).is_none());
    assert!(!is_signed_in(dir, "codex", None));

    // Read-only Codex CLI link
    let custom = tmp.path().join("codex-home").join("auth.json");
    std::fs::create_dir_all(custom.parent().unwrap()).unwrap();
    let access = make_jwt(json!({"exp": (now_unix() + 3600.0) as i64}));
    let id_tok = make_jwt(json!({
        "email": "cli@example.com",
        "https://api.openai.com/auth": {"chatgpt_account_id": "acct-cli", "chatgpt_plan_type": "pro"}
    }));
    std::fs::write(
        &custom,
        json!({
            "auth_mode": "chatgpt",
            "tokens": {"access_token": access, "refresh_token": "r", "id_token": id_tok, "account_id": "acct-cli"}
        })
        .to_string(),
    )
    .unwrap();

    link_codex_cli(dir, Some(&custom), None).unwrap();
    let stored_raw = std::fs::read_to_string(credential_path(dir, "codex")).unwrap();
    assert!(!stored_raw.contains("access_token"));
    assert!(!stored_raw.contains("refresh_token"));
    assert!(is_signed_in(dir, "codex", None));

    // Logging out of Codex CLI empties linked file -> is_signed_in flips to false!
    std::fs::write(&custom, r#"{"auth_mode":"chatgpt","tokens":null}"#).unwrap();
    assert!(!is_signed_in(dir, "codex", None));

    // API key auth_mode refused
    std::fs::write(&custom, r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-x"}"#).unwrap();
    assert!(
        link_codex_cli(dir, Some(&custom), None)
            .unwrap_err()
            .to_string()
            .contains("no ChatGPT sign-in")
    );
}

#[test]
fn test_antigravity_client_binary_sha256_scanner_and_browser_detection() {
    let tmp = TempDir::new().unwrap();
    // Assemble fake client ID & secret from pieces so repo holds no real credential patterns.
    let fake_id = format!("123456789012-{}.apps.googleusercontent.com", "a".repeat(32));
    let fake_sec = format!("GOC{}{}", "SPX-", "b".repeat(28));
    let other_id = format!("987654321098-{}.apps.googleusercontent.com", "c".repeat(32));
    let other_sec = format!("GOC{}{}", "SPX-", "d".repeat(28));

    let id_sha = hex::encode(Sha256::digest(fake_id.as_bytes()));
    let sec_sha = hex::encode(Sha256::digest(fake_sec.as_bytes()));

    let bin = tmp.path().join("language_server");
    let mut bytes = vec![0u8; 256];
    bytes.extend_from_slice(other_id.as_bytes());
    bytes.extend_from_slice(b"\x00proto3.21.12");
    bytes.extend_from_slice(fake_id.as_bytes());
    bytes.extend_from_slice(&vec![0u8; 256]);
    bytes.extend_from_slice(fake_sec.as_bytes());
    bytes.extend_from_slice(other_sec.as_bytes());
    std::fs::write(&bin, &bytes).unwrap();

    let (got_id, got_sec) = antigravity_client(
        None,
        tmp.path(),
        Some(std::slice::from_ref(&bin)),
        Some((&id_sha, &sec_sha)),
    )
    .unwrap();
    assert_eq!((got_id, got_sec), (fake_id.clone(), fake_sec.clone()));

    // Changed hash -> names the binary and mentions "may have changed it"
    let err = antigravity_client(
        None,
        tmp.path(),
        Some(std::slice::from_ref(&bin)),
        Some(("0000", "0000")),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("may have changed it"), "{err}");

    // Unreadable directory -> "could not be read", not "changed"
    let folder = tmp.path().join("dir_not_file");
    std::fs::create_dir(&folder).unwrap();
    let err2 = antigravity_client(None, tmp.path(), Some(&[folder]), Some((&id_sha, &sec_sha)))
        .unwrap_err()
        .to_string();
    assert!(
        err2.contains("could not be read") && !err2.contains("changed"),
        "{err2}"
    );

    // Half-configured client refused
    let mut half_cfg = Config::defaults_without_home();
    half_cfg.antigravity_client_id = Some(fake_id);
    let err3 = antigravity_client(Some(&half_cfg), tmp.path(), Some(&[]), None)
        .unwrap_err()
        .to_string();
    assert!(err3.contains("only one of"), "{err3}");

    // Browser detection across platforms
    assert!(can_open_browser_for("darwin", None, None, false));
    assert!(can_open_browser_for("windows", None, None, false));
    assert!(can_open_browser_for("linux", Some(":0"), None, true));
    assert!(can_open_browser_for("linux", None, Some("wayland-0"), true));
    assert!(!can_open_browser_for("linux", None, None, true));
    assert!(!can_open_browser_for("linux", Some(":0"), None, false));
}

#[tokio::test]
async fn test_concurrent_refresh_shares_one_request_and_loopback_login_e2e() {
    let server = MockServer::start().await;
    let refresh_hits = Arc::new(AtomicUsize::new(0));
    let r_c = Arc::clone(&refresh_hits);

    let fresh_access = make_jwt(json!({"exp": (now_unix() + 7200.0) as i64}));
    let fresh_id = make_jwt(json!({
        "email": "me@example.com",
        "https://api.openai.com/auth": {"chatgpt_account_id": "acct-1", "chatgpt_plan_type": "plus"}
    }));

    struct CountingToken {
        hits: Arc<AtomicUsize>,
        access: String,
        id_token: String,
    }
    impl wiremock::Respond for CountingToken {
        fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
            self.hits.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(json!({
                "access_token": self.access,
                "refresh_token": "r2",
                "id_token": self.id_token,
                "expires_in": 7200
            }))
        }
    }

    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(CountingToken {
            hits: r_c,
            access: fresh_access.clone(),
            id_token: fresh_id.clone(),
        })
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let mut cfg = Config::defaults_without_home();
    cfg.config_dir = tmp.path().to_path_buf();
    let mgr = Arc::new(OAuthManager::from_config(&cfg));
    let ports = free_ports(2);
    let mut spec = OAuthProvider::codex();
    spec.token_url = format!("{}/oauth/token", server.uri());
    spec.redirect_ports = ports.clone();
    mgr.override_provider(spec);

    // 1. Concurrent callers share ONE refresh
    mgr.save(&Credential {
        provider: "codex".into(),
        access_token: "old".into(),
        refresh_token: "r1".into(),
        expires_at: 0.0,
        account_id: "acct-1".into(),
        source: "oauth".into(),
        ..Credential::default()
    })
    .unwrap();
    let futs = (0..5).map(|_| {
        let m = Arc::clone(&mgr);
        async move { m.credential("codex", false).await.unwrap() }
    });
    let creds = futures::future::join_all(futs).await;
    assert_eq!(refresh_hits.load(Ordering::SeqCst), 1);
    assert!(creds.iter().all(|c| c.access_token == fresh_access));
    assert_eq!(mgr.load("codex").unwrap().refresh_token, "r2");

    // 2. Loopback login E2E: stray bad-state callback is rejected 400 and real callback succeeds 200!
    mgr.sign_out("codex");
    let (url_tx, url_rx) = tokio::sync::oneshot::channel::<String>();
    let url_tx = Arc::new(std::sync::Mutex::new(Some(url_tx)));

    let mgr_login = Arc::clone(&mgr);
    let login_task = tokio::spawn(async move {
        mgr_login
            .login(
                "codex",
                LoginOpts {
                    open_browser: false,
                    paste: false,
                    wait_seconds: 5.0,
                    on_url: Some(Box::new(move |u| {
                        if let Some(tx) = url_tx.lock().unwrap().take() {
                            let _ = tx.send(u.to_owned());
                        }
                    })),
                },
            )
            .await
    });

    let auth_url = tokio::time::timeout(Duration::from_secs(3), url_rx)
        .await
        .unwrap()
        .unwrap();
    let parsed = url::Url::parse(&auth_url).unwrap();
    let state = parsed
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    let port = parsed
        .query_pairs()
        .find(|(k, _)| k == "redirect_uri")
        .and_then(|(_, v)| url::Url::parse(&v).ok())
        .and_then(|u| u.port())
        .unwrap();

    // Stray forged callback -> 400 Bad Request ("ignored"), login keeps waiting!
    let (st_forged, body_forged) = http_get(port, "/auth/callback?code=FORGED&state=wrong").await;
    assert!(st_forged.contains("400"), "{st_forged}");
    assert!(body_forged.contains("ignored"), "{body_forged}");

    // Real callback -> 200 OK ("Signed in")
    let (st_ok, body_ok) = http_get(port, &format!("/auth/callback?code=REAL&state={state}")).await;
    assert!(st_ok.contains("200"), "{st_ok}");
    assert!(body_ok.contains("Signed in"), "{body_ok}");

    let cred = login_task.await.unwrap().unwrap();
    assert_eq!(cred.email, "me@example.com");
    assert_eq!(cred.account_id, "acct-1");
    assert!(mgr.is_signed_in("codex"));

    // 3. Login CLI status / logout / antigravity warning output
    let st_out = login::run_status(&mgr);
    assert_eq!(st_out.exit_code, 0);
    assert!(st_out.stdout.contains("me@example.com") && !st_out.stdout.contains(&fresh_access));

    let lo_out = login::run_logout(&mgr, "codex");
    assert_eq!(lo_out.exit_code, 0);
    assert!(lo_out.stdout.contains("Signed out of codex"));
    assert!(mgr.load("codex").is_none());

    // Antigravity warning printed BEFORE any error/url
    let ag_out = login::run_login(&mgr, "antigravity", true, 1.0, |_| {}).await;
    assert_eq!(ag_out.exit_code, 1); // no client installed in temp config
    assert!(ag_out.stdout.contains("Warning:") && ag_out.stdout.contains("suspension"));
}

#[tokio::test]
async fn test_first_use_no_browser_never_retries() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = Config::defaults_without_home();
    cfg.config_dir = tmp.path().to_path_buf();
    let mgr = Arc::new(OAuthManager::from_config(&cfg));
    let mut spec = OAuthProvider::codex();
    spec.redirect_ports = free_ports(2);
    mgr.override_provider(spec);

    let opens = Arc::new(AtomicUsize::new(0));
    let opens_c = Arc::clone(&opens);
    mgr.set_browser_opener(move |_| {
        opens_c.fetch_add(1, Ordering::SeqCst);
        false
    });

    // Wait briefly for first attempt to fail because browser could not open.
    let e1 = mgr.sign_in_on_first_use("codex", 1.0).await.unwrap_err();
    assert!(
        e1.to_string().contains("no browser") || e1.is_pending(),
        "{e1}"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = mgr.sign_in_on_first_use("codex", 0.2).await;
    assert_eq!(
        opens.load(Ordering::SeqCst),
        1,
        "must not launch browser twice"
    );
}

#[tokio::test]
async fn test_oauth_rejects_path_traversal_provider_id_and_multibyte_error_body() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let oauth_dir = dir.join("oauth");

    for bad in [
        "../../evil",
        "../codex",
        "",
        "CODEX",
        "bad/slash",
        &"a".repeat(65),
    ] {
        let cred = Credential {
            provider: bad.to_owned(),
            access_token: "secret".into(),
            ..Credential::default()
        };
        assert!(save(dir, &cred).is_err(), "save must reject {bad:?}");
        assert!(load(dir, bad, None).is_none(), "load must reject {bad:?}");
        assert!(!sign_out(dir, bad), "sign_out must reject {bad:?}");
        assert!(
            !is_signed_in(dir, bad, None),
            "is_signed_in must reject {bad:?}"
        );
        let p = credential_path(dir, bad);
        assert_eq!(
            p.parent(),
            Some(oauth_dir.as_path()),
            "credential_path({bad:?}) escaped oauth dir: {}",
            p.display()
        );
    }

    // Multibyte UTF-8 error body spanning byte 240 from token refresh must not panic.
    let server = MockServer::start().await;
    let body = format!("{}€€€€€", "e".repeat(239));
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_string(body))
        .mount(&server)
        .await;

    let mut cfg = Config::defaults_without_home();
    cfg.config_dir = dir.to_path_buf();
    let mgr = Arc::new(OAuthManager::from_config(&cfg));
    let mut spec = OAuthProvider::codex();
    spec.token_url = format!("{}/token", server.uri());
    mgr.override_provider(spec);
    mgr.save(&Credential {
        provider: "codex".into(),
        access_token: "old".into(),
        refresh_token: "ref".into(),
        expires_at: 1.0,
        ..Credential::default()
    })
    .unwrap();

    let err = mgr
        .credential("codex", true)
        .await
        .expect_err("HTTP 400 refresh must fail cleanly without UTF-8 panic");
    assert!(err.to_string().contains("400"), "got {err}");
}
