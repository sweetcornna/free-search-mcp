//! CLI `free-search-mcp login ...` flow (port of `src/search_mcp/login.py`).

use crate::oauth::{
    ANTIGRAVITY_WARNING, Credential, LoginOpts, OAuthError, OAuthManager, link_codex_cli,
};
use std::path::Path;
use std::sync::Arc;

/// Output returned by a login CLI subcommand (pure text so tests and CLI share one implementation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Format sign-in status across `codex` and `antigravity` (never includes tokens).
pub fn run_status(mgr: &OAuthManager) -> LoginOutput {
    let mut lines = Vec::new();
    for id in ["codex", "antigravity"] {
        match mgr.status(id) {
            Ok(st) => {
                if st.signed_in {
                    let who = st.email.as_deref().unwrap_or("(signed in)");
                    let mut extra = Vec::new();
                    if let Some(plan) = &st.plan {
                        extra.push(format!("plan: {plan}"));
                    }
                    if let Some(src) = &st.source {
                        extra.push(format!("source: {src}"));
                    }
                    if st.expired == Some(true) {
                        extra.push("expired".to_owned());
                    }
                    let suffix = if extra.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", extra.join(", "))
                    };
                    lines.push(format!("{id:12} signed in as {who}{suffix}"));
                } else {
                    lines.push(format!(
                        "{id:12} not signed in  (run `free-search-mcp login {id}`)"
                    ));
                }
            }
            Err(e) => {
                lines.push(format!("{id:12} error: {e}"));
            }
        }
    }
    LoginOutput {
        exit_code: 0,
        stdout: lines.join("\n") + "\n",
        stderr: String::new(),
    }
}

/// Sign out of a provider.
pub fn run_logout(mgr: &OAuthManager, provider_id: &str) -> LoginOutput {
    if let Err(e) = mgr.provider(provider_id) {
        return LoginOutput {
            exit_code: 1,
            stdout: String::new(),
            stderr: format!("{e}\n"),
        };
    }
    if mgr.sign_out(provider_id) {
        LoginOutput {
            exit_code: 0,
            stdout: format!("Signed out of {provider_id}.\n"),
            stderr: String::new(),
        }
    } else {
        LoginOutput {
            exit_code: 0,
            stdout: format!("{provider_id} was not signed in.\n"),
            stderr: String::new(),
        }
    }
}

/// Link read-only to an existing Codex CLI sign-in (`auth.json`).
pub fn run_link_codex_cli(mgr: &OAuthManager, auth_path: Option<&Path>) -> LoginOutput {
    match link_codex_cli(&mgr.config_dir, auth_path, mgr.codex_home.as_deref()) {
        Ok(cred) => {
            let mut out = String::from("codex: linked to the Codex CLI sign-in (read-only).\n");
            out.push_str(&format_signed_in_summary("codex", &cred));
            LoginOutput {
                exit_code: 0,
                stdout: out,
                stderr: String::new(),
            }
        }
        Err(e) => LoginOutput {
            exit_code: 1,
            stdout: String::new(),
            stderr: format!("{e}\n"),
        },
    }
}

/// Run interactive browser / paste-back login for `codex` or `antigravity`.
pub async fn run_login(
    mgr: &Arc<OAuthManager>,
    provider_id: &str,
    no_browser: bool,
    wait_seconds: f64,
    emit: impl Fn(&str) + Send + Sync + 'static,
) -> LoginOutput {
    let mut stdout = String::new();
    let emit = Arc::new(emit);

    if provider_id == "antigravity" {
        let warn = format!("{ANTIGRAVITY_WARNING}\n\n");
        emit(&warn);
        stdout.push_str(&warn);
    }

    let captured = Arc::new(std::sync::Mutex::new(String::new()));
    let cap_clone = Arc::clone(&captured);
    let emit_clone = Arc::clone(&emit);
    let spec_hint = mgr.provider(provider_id).ok();

    let res = mgr
        .login(
            provider_id,
            LoginOpts {
                open_browser: !no_browser,
                paste: no_browser,
                wait_seconds,
                on_url: Some(Box::new(move |url| {
                    let mut msg = format!("Open this link to sign in:\n  {url}\n");
                    if no_browser
                        && let Some(ref spec) = spec_hint
                    {
                        let port = url::Url::parse(url)
                            .ok()
                            .and_then(|u| {
                                u.query_pairs()
                                    .find(|(k, _)| k == "redirect_uri")
                                    .and_then(|(_, v)| url::Url::parse(&v).ok())
                                    .and_then(|ru| ru.port())
                            })
                            .unwrap_or_else(|| *spec.redirect_ports.first().unwrap_or(&1455));
                        msg.push_str(&format!(
                            "Over SSH/headless, paste the redirected address here (starts with {}?code=...):\n",
                            spec.redirect_uri(port)
                        ));
                    }
                    emit_clone(&msg);
                    if let Ok(mut buf) = cap_clone.lock() {
                        buf.push_str(&msg);
                    }
                })),
            },
        )
        .await;

    if let Ok(buf) = captured.lock() {
        stdout.push_str(&buf);
    }

    match res {
        Ok(cred) => {
            let summary = format_signed_in_summary(provider_id, &cred);
            emit(&summary);
            stdout.push_str(&summary);
            LoginOutput {
                exit_code: 0,
                stdout,
                stderr: String::new(),
            }
        }
        Err(err) => {
            let msg = format!("{err}\n");
            LoginOutput {
                exit_code: 1,
                stdout,
                stderr: msg,
            }
        }
    }
}

fn format_signed_in_summary(provider_id: &str, cred: &Credential) -> String {
    let who = if cred.email.is_empty() {
        provider_id.to_owned()
    } else {
        cred.email.clone()
    };
    let plan = if cred.plan.is_empty() {
        String::new()
    } else {
        format!(" (plan: {})", cred.plan)
    };
    format!(
        "signed in as {who}{plan}\nUse it in search via sources=[\"{provider_id}\"] (or automatic agent lane for codex).\n"
    )
}

/// Format an `OAuthError` concisely.
pub fn format_oauth_error(err: &OAuthError) -> String {
    err.to_string()
}
