"""The sign-in module behind the `codex` engine.

Offline: the token endpoint is an `httpx.MockTransport`, and the loopback
callback is exercised by connecting to it from the test itself.
"""
from __future__ import annotations

import asyncio
import base64
import hashlib
import json
import os
import socket
import stat
import subprocess
import time
from pathlib import Path
from urllib.parse import parse_qs, urlparse

import httpx
import pytest

from search_mcp import oauth

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.


def _jwt(claims: dict) -> str:
    def seg(data: dict) -> str:
        return base64.urlsafe_b64encode(json.dumps(data).encode()).rstrip(b"=").decode()

    return f"{seg({'alg': 'none'})}.{seg(claims)}.sig"


def _tokens(exp: float, *, refresh: str = "r1", email: str = "me@example.com") -> dict:
    return {
        "access_token": _jwt({"exp": int(exp)}),
        "refresh_token": refresh,
        "id_token": _jwt(
            {
                "email": email,
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "acct-1",
                    "chatgpt_plan_type": "plus",
                },
            }
        ),
    }


def _stored(**fields) -> oauth.Credential:
    base = {"provider": "codex", "access_token": "a", "refresh_token": "r",
            "expires_at": time.time() + 3600, "account_id": "acct-1"}
    cred = oauth.Credential(**{**base, **fields})
    oauth.save(cred)
    return cred


@pytest.fixture
def mock_http(monkeypatch):
    """Route oauth's HTTP through a handler the test supplies."""
    calls: list[httpx.Request] = []
    state: dict = {"handler": None}

    def factory(engine=None, *, timeout=30.0):
        def handle(request: httpx.Request) -> httpx.Response:
            calls.append(request)
            return state["handler"](request)

        return httpx.AsyncClient(transport=httpx.MockTransport(handle))

    monkeypatch.setattr(oauth, "http_client", factory)
    return state, calls


def _body(request: httpx.Request) -> dict[str, str]:
    raw = request.content.decode()
    try:
        return json.loads(raw)
    except ValueError:
        return {k: v[0] for k, v in parse_qs(raw).items()}


# --- the pieces --------------------------------------------------------------------


def test_pkce_challenge_is_the_s256_of_the_verifier():
    verifier, challenge = oauth.pkce_pair()
    assert 43 <= len(verifier) <= 128
    digest = hashlib.sha256(verifier.encode()).digest()
    assert challenge == base64.urlsafe_b64encode(digest).rstrip(b"=").decode()


def test_the_authorize_link_is_the_codex_cli_one():
    spec = oauth.CODEX
    link = urlparse(spec.authorize_link("CHALLENGE", "STATE", spec.redirect_uri(1455)))
    assert f"{link.scheme}://{link.netloc}{link.path}" == "https://auth.openai.com/oauth/authorize"
    query = {k: v[0] for k, v in parse_qs(link.query).items()}
    assert query["client_id"] == "app_EMoamEEZ73f0CkXaXp7hrann"
    assert query["redirect_uri"] == "http://127.0.0.1:1455/auth/callback"
    assert query["code_challenge"] == "CHALLENGE"
    assert query["code_challenge_method"] == "S256"
    assert query["state"] == "STATE"
    assert query["scope"] == "openid profile email offline_access"
    assert query["codex_cli_simplified_flow"] == "true"
    assert query["id_token_add_organizations"] == "true"
    assert "+" not in link.query  # spaces are %20, not form-style


def test_only_the_registered_redirect_ports_are_used():
    # Both are on the Codex client's redirect allow-list; no other port works.
    assert oauth.CODEX.redirect_ports == (1455, 1457)


def test_a_redirect_is_accepted_only_with_the_matching_state():
    assert oauth.read_redirect({"code": ["abc"], "state": ["s1"]}, "s1") == "abc"
    # ChatGPT may append an onboarding suffix to the state.
    assert oauth.read_redirect({"code": ["abc"], "state": ["s1-suffix"]}, "s1") == "abc"
    with pytest.raises(oauth.OAuthError, match="state mismatch"):
        oauth.read_redirect({"code": ["abc"], "state": ["other"]}, "s1")
    with pytest.raises(oauth.OAuthError, match="state mismatch"):
        oauth.read_redirect({"code": ["abc"], "state": [""]}, "")
    with pytest.raises(oauth.OAuthError, match="access_denied"):
        oauth.read_redirect({"error": ["access_denied"], "state": ["s1"]}, "s1")
    with pytest.raises(oauth.OAuthError, match="no authorization code"):
        oauth.read_redirect({"state": ["s1"]}, "s1")


def test_a_workspace_without_codex_is_named_as_such():
    params = {"error": ["access_denied"],
              "error_description": ["missing_codex_entitlement"], "state": ["s"]}
    with pytest.raises(oauth.OAuthError, match="does not include Codex"):
        oauth.read_redirect(params, "s")


def test_a_pasted_redirect_address_yields_its_code():
    pasted = "http://127.0.0.1:1455/auth/callback?code=xyz&state=s1\n"
    assert oauth.parse_pasted(pasted, "s1") == "xyz"
    assert oauth.parse_pasted("http://localhost:1455/auth/callback?code=xyz&state=s1", "s1") == "xyz"
    assert oauth.parse_pasted("?code=xyz&state=s1", "s1") == "xyz"
    with pytest.raises(oauth.OAuthError, match="code="):
        oauth.parse_pasted("just some text", "s1")


def test_jwt_claims_tolerates_garbage():
    assert oauth.jwt_claims("not-a-jwt") == {}
    assert oauth.jwt_claims(_jwt({"exp": 5})) == {"exp": 5}


def test_the_workspace_id_and_email_come_from_the_id_token_claims():
    cred = oauth.Credential(provider="codex", id_token=_jwt({
        "https://api.openai.com/profile": {"email": "p@example.com"},
        "organizations": [{"id": "org-first"}],
    }))
    oauth._apply_codex_claims(cred)
    # Fallbacks when the namespaced auth claim is missing.
    assert cred.email == "p@example.com" and cred.account_id == "org-first"


# --- the store -----------------------------------------------------------------------


def test_nothing_is_signed_in_by_default():
    assert oauth.load("codex") is None
    assert oauth.status("codex") == {
        "provider": "codex",
        "label": oauth.CODEX.label,
        "engine": "codex",
        "signed_in": False,
    }


def test_a_saved_credential_is_private_and_round_trips():
    _stored(email="me@example.com", plan="plus")
    path = oauth.credential_path("codex")
    if os.name == "posix":
        assert stat.S_IMODE(path.stat().st_mode) == 0o600
        assert stat.S_IMODE(path.parent.stat().st_mode) == 0o700
    loaded = oauth.load("codex")
    assert loaded is not None
    assert (loaded.access_token, loaded.refresh_token, loaded.account_id) == ("a", "r", "acct-1")
    public = oauth.status("codex")
    assert public["signed_in"] and public["email"] == "me@example.com"
    assert "access_token" not in public and "refresh_token" not in public


def test_status_never_contains_a_token():
    tokens = _tokens(time.time() + 3600, refresh="REFRESH-SECRET")
    cred = oauth.Credential(provider="codex", access_token=tokens["access_token"],
                            refresh_token=tokens["refresh_token"], id_token=tokens["id_token"])
    oauth._apply_codex_claims(cred)
    oauth.save(cred)
    dumped = json.dumps(oauth.status("codex"))
    assert tokens["access_token"] not in dumped
    assert "REFRESH-SECRET" not in dumped
    assert "me@example.com" in dumped and "plus" in dumped


def test_a_corrupt_file_reads_as_signed_out():
    path = oauth.credential_path("codex")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("{not json", encoding="utf-8")
    assert oauth.load("codex") is None
    assert oauth.is_signed_in("codex") is False


def test_sign_out_deletes_the_file():
    _stored()
    assert oauth.sign_out("codex") is True
    assert oauth.load("codex") is None
    assert oauth.sign_out("codex") is False


def test_an_unknown_provider_is_a_value_error():
    with pytest.raises(ValueError, match="use codex, antigravity"):
        oauth.provider("gemini-cli")


def test_opt_in_engines_reports_a_sign_in():
    from search_mcp.keystore import opt_in_engines

    assert opt_in_engines()["codex"] is False
    _stored()
    assert opt_in_engines()["codex"] is True


# --- refresh ---------------------------------------------------------------------------


async def test_a_fresh_credential_is_returned_without_a_request(mock_http):
    _, calls = mock_http
    _stored()
    cred = await oauth.credential("codex")
    assert cred.access_token == "a"
    assert calls == []


async def test_an_expiring_token_is_refreshed_the_way_the_cli_does_it(mock_http):
    state, calls = mock_http
    new = _tokens(time.time() + 7200, refresh="r2")
    state["handler"] = lambda request: httpx.Response(200, json=new)
    _stored(access_token="old", refresh_token="r1", expires_at=time.time() + 10)

    cred = await oauth.credential("codex")

    assert len(calls) == 1
    sent = calls[0]
    assert str(sent.url) == "https://auth.openai.com/oauth/token"
    # A JSON body with exactly these three fields, as codex-rs sends it.
    assert sent.headers["content-type"].startswith("application/json")
    assert json.loads(sent.content) == {
        "client_id": "app_EMoamEEZ73f0CkXaXp7hrann",
        "grant_type": "refresh_token",
        "refresh_token": "r1",
    }
    assert cred.access_token == new["access_token"]
    # OpenAI rotates refresh tokens; losing the new one signs the server out.
    assert oauth.load("codex").refresh_token == "r2"
    assert cred.account_id == "acct-1" and cred.email == "me@example.com"
    assert cred.expires_at > time.time() + 7000


async def test_a_refresh_reply_without_a_refresh_token_keeps_the_old_one(mock_http):
    state, _ = mock_http
    new = _tokens(time.time() + 7200)
    del new["refresh_token"]
    state["handler"] = lambda request: httpx.Response(200, json=new)
    _stored(refresh_token="keep", expires_at=0)
    cred = await oauth.credential("codex")
    assert cred.refresh_token == "keep"


async def test_a_revoked_refresh_token_says_to_sign_in_again(mock_http):
    state, _ = mock_http
    state["handler"] = lambda request: httpx.Response(
        401, json={"error": {"code": "refresh_token_reused", "message": "already used"}}
    )
    _stored(expires_at=0)
    with pytest.raises(oauth.OAuthError, match="search-mcp-login codex"):
        await oauth.credential("codex")


async def test_an_unreachable_token_host_suggests_the_proxy(mock_http):
    state, _ = mock_http

    def refuse(request):
        raise httpx.ConnectError("refused", request=request)

    state["handler"] = refuse
    _stored(expires_at=0)
    with pytest.raises(oauth.OAuthError, match="SEARCH_MCP_PROXY"):
        await oauth.credential("codex")


async def test_concurrent_callers_share_one_refresh(mock_http):
    state, calls = mock_http
    state["handler"] = lambda request: httpx.Response(
        200, json=_tokens(time.time() + 7200, refresh="r2")
    )
    _stored(access_token="old", expires_at=0)
    creds = await asyncio.gather(*(oauth.credential("codex") for _ in range(5)))
    assert len(calls) == 1
    assert len({c.access_token for c in creds}) == 1


async def test_a_forced_refresh_runs_even_when_the_token_looks_fresh(mock_http):
    state, calls = mock_http
    state["handler"] = lambda request: httpx.Response(
        200, json=_tokens(time.time() + 7200, refresh="r2")
    )
    _stored()
    await oauth.credential("codex", force_refresh=True)
    assert len(calls) == 1


async def test_not_signed_in_is_its_own_error():
    with pytest.raises(oauth.NotSignedIn):
        await oauth.credential("codex")


# --- the Codex CLI link ------------------------------------------------------------------


def _write_codex_cli(exp: float, **extra) -> dict:
    tokens = _tokens(exp)
    path = oauth.codex_cli_auth_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": None,
        "tokens": {**tokens, "account_id": "acct-1"},
        "last_refresh": "2026-09-20T00:00:00Z",
        **extra,
    }), encoding="utf-8")
    return tokens


async def test_the_codex_cli_link_reads_tokens_and_stores_none(mock_http):
    _, calls = mock_http
    tokens = _write_codex_cli(time.time() + 3600)
    oauth.link_codex_cli()
    stored = json.loads(oauth.credential_path("codex").read_text(encoding="utf-8"))
    assert stored["source"] == "codex-cli"
    assert "access_token" not in stored and "refresh_token" not in stored
    cred = await oauth.credential("codex")
    assert cred.access_token == tokens["access_token"]
    assert cred.account_id == "acct-1" and cred.plan == "plus"
    assert oauth.status("codex")["source"] == "codex-cli"
    assert calls == []


async def test_an_expired_codex_cli_link_is_never_refreshed_here(mock_http):
    _, calls = mock_http
    _write_codex_cli(time.time() - 60)
    oauth.link_codex_cli()
    before = oauth.codex_cli_auth_path().read_bytes()
    with pytest.raises(oauth.OAuthError, match="Run `codex` once"):
        await oauth.credential("codex")
    with pytest.raises(oauth.OAuthError, match="was rejected"):
        await oauth.credential("codex", force_refresh=True)
    assert calls == []
    assert oauth.codex_cli_auth_path().read_bytes() == before


def test_linking_without_a_codex_cli_sign_in_says_how_to_get_one():
    with pytest.raises(oauth.OAuthError, match="codex login"):
        oauth.link_codex_cli()


def test_an_api_key_codex_login_is_not_a_chatgpt_sign_in():
    _write_codex_cli(time.time() + 3600, auth_mode="apikey", OPENAI_API_KEY="sk-x")
    with pytest.raises(oauth.OAuthError, match="no ChatGPT sign-in"):
        oauth.link_codex_cli()


# --- the browser flow ------------------------------------------------------------------------


async def _get(port: int, target: str) -> tuple[str, str]:
    reader, writer = await asyncio.open_connection("127.0.0.1", port)
    writer.write(f"GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".encode())
    await writer.drain()
    raw = (await reader.read()).decode()
    writer.close()
    head, _, body = raw.partition("\r\n\r\n")
    return head.split("\r\n", 1)[0], body


def _free_ports(n: int) -> list[int]:
    socks = [socket.socket() for _ in range(n)]
    try:
        for sock in socks:
            sock.bind(("127.0.0.1", 0))
        return [sock.getsockname()[1] for sock in socks]
    finally:
        for sock in socks:
            sock.close()


@pytest.fixture
def ports(monkeypatch):
    """Point the Codex provider at free ports; 1455 may be taken on a dev box."""
    chosen = tuple(_free_ports(2))
    spec = oauth.OAuthProvider(**{**oauth.CODEX.__dict__, "redirect_ports": chosen})
    monkeypatch.setitem(oauth.PROVIDERS, "codex", spec)
    monkeypatch.setattr(oauth, "CODEX", spec)
    return chosen


class _Browser:
    """An `on_url` that plays the browser: it follows the link's redirect.

    `seen` holds the link, then each reply as `(status line, page)`. The tab's
    reply comes only after the token exchange, so a test awaits `closed()`
    rather than guessing how long that takes.
    """

    def __init__(self, *visit_paths):
        self.visit_paths = visit_paths
        self.seen: list = []
        self.tasks: list[asyncio.Task] = []

    def __call__(self, url: str) -> None:
        self.seen.append(url)
        query = parse_qs(urlparse(url).query)
        port = urlparse(query["redirect_uri"][0]).port

        async def visit() -> None:
            await asyncio.sleep(0.05)
            for path in self.visit_paths:
                self.seen.append(await _get(port, path(query["state"][0])))

        self.tasks.append(asyncio.get_running_loop().create_task(visit()))

    async def closed(self) -> None:
        await asyncio.wait_for(asyncio.gather(*self.tasks), 5)


def _browser(visit_path):
    browser = _Browser(visit_path)
    return browser, browser.seen


async def test_the_loopback_callback_completes_a_sign_in(mock_http, ports):
    state, calls = mock_http
    state["handler"] = lambda request: httpx.Response(200, json=_tokens(time.time() + 3600))
    on_url, seen = _browser(lambda s: f"/auth/callback?code=THE-CODE&state={s}")

    cred = await oauth.login("codex", open_browser=False, on_url=on_url, wait_seconds=5)
    await on_url.closed()

    assert cred.email == "me@example.com" and cred.account_id == "acct-1"
    link, (status, page) = seen[0], seen[1]
    assert status.endswith("200 OK") and "Signed in" in page
    sent = calls[0]
    assert sent.headers["content-type"] == "application/x-www-form-urlencoded"
    body = _body(sent)
    assert body["grant_type"] == "authorization_code"
    assert body["code"] == "THE-CODE"
    assert body["redirect_uri"] == f"http://127.0.0.1:{ports[0]}/auth/callback"
    challenge = parse_qs(urlparse(link).query)["code_challenge"][0]
    digest = hashlib.sha256(body["code_verifier"].encode()).digest()
    assert base64.urlsafe_b64encode(digest).rstrip(b"=").decode() == challenge
    assert oauth.is_signed_in("codex")


async def test_a_busy_first_port_falls_back_to_the_second(mock_http, ports):
    state, calls = mock_http
    state["handler"] = lambda request: httpx.Response(200, json=_tokens(time.time() + 3600))
    blocker = await asyncio.start_server(lambda r, w: None, "127.0.0.1", ports[0])
    try:
        on_url, _ = _browser(lambda s: f"/auth/callback?code=C&state={s}")
        await oauth.login("codex", open_browser=False, on_url=on_url, wait_seconds=5)
        await on_url.closed()
    finally:
        blocker.close()
    assert _body(calls[0])["redirect_uri"] == f"http://127.0.0.1:{ports[1]}/auth/callback"


async def test_a_stray_callback_is_refused_and_the_sign_in_keeps_waiting(mock_http, ports):
    # Any web page can make the browser request the callback address. A reply
    # with the wrong state must not end the sign-in the operator is doing.
    state, calls = mock_http
    state["handler"] = lambda request: httpx.Response(200, json=_tokens(time.time() + 3600))
    browser = _Browser(
        lambda s: "/auth/callback?code=FORGED&state=forged",
        lambda s: "/auth/callback?error=access_denied&state=forged",
        lambda s: f"/auth/callback?code=REAL&state={s}",
    )

    cred = await oauth.login("codex", open_browser=False, on_url=browser, wait_seconds=5)
    await browser.closed()
    seen = browser.seen[1:]
    assert [status.split(" ", 1)[1] for status, _ in seen] == [
        "400 Bad Request", "400 Bad Request", "200 OK"
    ]
    assert "ignored" in seen[0][1]
    assert [_body(c)["code"] for c in calls] == ["REAL"]
    assert cred.account_id == "acct-1" and oauth.is_signed_in("codex")


async def test_a_refusal_with_the_right_state_ends_the_sign_in(mock_http, ports):
    _, calls = mock_http
    on_url, seen = _browser(lambda s: f"/auth/callback?error=access_denied&state={s}")
    with pytest.raises(oauth.OAuthError, match="access_denied"):
        await oauth.login("codex", open_browser=False, on_url=on_url, wait_seconds=5)
    await on_url.closed()
    assert seen[1][0].endswith("400 Bad Request")
    assert calls == []
    assert not oauth.is_signed_in("codex")


async def test_the_browser_tab_shows_a_failed_exchange_not_success(mock_http, ports):
    state, _ = mock_http
    state["handler"] = lambda request: httpx.Response(
        401, json={"error": {"message": "Could not validate your token."}}
    )
    on_url, seen = _browser(lambda s: f"/auth/callback?code=BAD&state={s}")
    with pytest.raises(oauth.OAuthError, match="HTTP 401"):
        await oauth.login("codex", open_browser=False, on_url=on_url, wait_seconds=5)
    await on_url.closed()
    status, page = seen[1]
    assert status.endswith("400 Bad Request")
    assert "Sign-in failed" in page and "Could not validate your token" in page


async def test_a_slow_exchange_leaves_the_tab_undecided_not_signed_in(mock_http, ports,
                                                                    monkeypatch):
    monkeypatch.setattr(oauth, "_REPLY_WAIT", 0.05)

    async def slow_then_failing(client, spec, form):
        await asyncio.sleep(0.3)
        raise oauth.OAuthError("token endpoint answered HTTP 500")

    monkeypatch.setattr(oauth, "_token_request", slow_then_failing)
    on_url, seen = _browser(lambda s: f"/auth/callback?code=C&state={s}")
    with pytest.raises(oauth.OAuthError, match="HTTP 500"):
        await oauth.login("codex", open_browser=False, on_url=on_url, wait_seconds=5)
    await on_url.closed()
    status, page = seen[1]
    assert status.endswith("202 Accepted")
    assert "Signed in" not in page and "Still finishing" in page


async def test_a_sign_in_without_a_workspace_id_is_refused(mock_http, ports):
    state, _ = mock_http
    tokens = _tokens(time.time() + 3600)
    tokens["id_token"] = _jwt({"email": "me@example.com"})
    state["handler"] = lambda request: httpx.Response(200, json=tokens)
    on_url, _ = _browser(lambda s: f"/auth/callback?code=C&state={s}")
    with pytest.raises(oauth.OAuthError, match="workspace id"):
        await oauth.login("codex", open_browser=False, on_url=on_url, wait_seconds=5)
    await on_url.closed()
    assert not oauth.is_signed_in("codex")


async def test_busy_ports_are_reported_by_number(ports):
    blockers = [await asyncio.start_server(lambda r, w: None, "127.0.0.1", p) for p in ports]
    try:
        with pytest.raises(oauth.OAuthError, match=f"port {ports[0]} and {ports[1]}"):
            await oauth.login("codex", open_browser=False, wait_seconds=1)
    finally:
        for blocker in blockers:
            blocker.close()


# --- sign-in on first use ----------------------------------------------------------------


def _visit_blocking(port: int, target: str) -> None:
    """A browser's GET, from a plain thread. A raw socket, because the suite's
    DNS stub answers even `127.0.0.1` with a public address."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.settimeout(5)
        sock.connect(("127.0.0.1", port))
        sock.sendall(f"GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".encode())
        while sock.recv(4096):
            pass


def _approving_browser(monkeypatch, *, approve: bool = True, delay: float = 0.05):
    """Stand in for the desktop browser: record each open, and optionally
    follow the link to the callback the way a signed-in user's browser does."""
    import threading

    opened: list[str] = []

    def fake_open(url: str) -> bool:
        opened.append(url)
        if approve:
            query = parse_qs(urlparse(url).query)
            redirect = urlparse(query["redirect_uri"][0])

            def visit() -> None:
                time.sleep(delay)
                _visit_blocking(redirect.port, f"{redirect.path}?code=C&state={query['state'][0]}")

            threading.Thread(target=visit, daemon=True).start()
        return True

    monkeypatch.setattr(oauth, "open_in_browser", fake_open)
    return opened


async def test_first_use_opens_the_page_and_returns_once_it_is_approved(mock_http, ports,
                                                                        monkeypatch):
    state, _ = mock_http
    state["handler"] = lambda request: httpx.Response(200, json=_tokens(time.time() + 3600))
    opened = _approving_browser(monkeypatch)

    cred = await oauth.sign_in_on_first_use("codex", wait_seconds=5)

    assert len(opened) == 1 and opened[0].startswith("https://auth.openai.com/oauth/authorize?")
    assert cred.email == "me@example.com"
    assert oauth.is_signed_in("codex")


async def test_a_slow_approval_is_pending_and_never_opens_a_second_page(mock_http, ports,
                                                                        monkeypatch):
    state, _ = mock_http
    state["handler"] = lambda request: httpx.Response(200, json=_tokens(time.time() + 3600))
    opened = _approving_browser(monkeypatch, delay=0.6)

    with pytest.raises(oauth.SignInPending, match="not yet approved"):
        await oauth.sign_in_on_first_use("codex", wait_seconds=0.1)
    # The next search waits on the same page, and gets the sign-in.
    cred = await oauth.sign_in_on_first_use("codex", wait_seconds=5)
    assert len(opened) == 1
    assert cred.account_id == "acct-1"


async def test_no_browser_means_no_sign_in_and_no_retry(mock_http, ports, monkeypatch):
    calls: list[str] = []

    def no_browser(url: str) -> bool:
        calls.append(url)
        return False

    monkeypatch.setattr(oauth, "open_in_browser", no_browser)
    with pytest.raises(oauth.OAuthError, match="no browser could be opened"):
        await oauth.sign_in_on_first_use("codex", wait_seconds=5)
    with pytest.raises(oauth.OAuthError, match="not opened again by itself"):
        await oauth.sign_in_on_first_use("codex", wait_seconds=5)
    assert len(calls) == 1


async def test_a_refused_first_use_is_not_opened_again(mock_http, ports, monkeypatch):
    import threading

    opened: list[str] = []

    def refusing_browser(url: str) -> bool:
        opened.append(url)
        query = parse_qs(urlparse(url).query)
        redirect = urlparse(query["redirect_uri"][0])

        def visit() -> None:
            time.sleep(0.05)
            _visit_blocking(redirect.port,
                            f"{redirect.path}?error=access_denied&state={query['state'][0]}")

        threading.Thread(target=visit, daemon=True).start()
        return True

    monkeypatch.setattr(oauth, "open_in_browser", refusing_browser)
    with pytest.raises(oauth.OAuthError, match="access_denied"):
        await oauth.sign_in_on_first_use("codex", wait_seconds=5)
    with pytest.raises(oauth.OAuthError, match="not opened again"):
        await oauth.sign_in_on_first_use("codex", wait_seconds=5)
    assert len(opened) == 1


def test_the_browser_is_started_with_every_stream_detached(monkeypatch):
    # Over stdio, stdout IS the MCP connection. A browser writing to it would
    # corrupt the protocol, so nothing it prints may reach the server's streams.
    import subprocess

    launched: list = []

    class FakePopen:
        def __init__(self, command, **kwargs):
            launched.append((command, kwargs))

    monkeypatch.setattr(oauth.sys, "platform", "linux")
    monkeypatch.setattr(subprocess, "Popen", FakePopen)
    assert oauth.open_in_browser("https://auth.openai.com/x") is True
    (command, kwargs), = launched
    assert command == ["xdg-open", "https://auth.openai.com/x"]
    assert kwargs["stdin"] is kwargs["stdout"] is kwargs["stderr"] is subprocess.DEVNULL
    assert kwargs["start_new_session"] is True


@pytest.mark.parametrize(
    ("platform", "env", "xdg", "expected"),
    [
        ("darwin", {}, None, True),
        ("win32", {}, None, True),
        ("linux", {"DISPLAY": ":0"}, "/usr/bin/xdg-open", True),
        ("linux", {"WAYLAND_DISPLAY": "wayland-0"}, "/usr/bin/xdg-open", True),
        ("linux", {}, "/usr/bin/xdg-open", False),  # a server or a container
        ("linux", {"DISPLAY": ":0"}, None, False),
    ],
)
def test_a_browser_is_only_assumed_where_one_can_start(monkeypatch, platform, env, xdg,
                                                        expected):
    monkeypatch.setattr(oauth.sys, "platform", platform)
    monkeypatch.delenv("DISPLAY", raising=False)
    monkeypatch.delenv("WAYLAND_DISPLAY", raising=False)
    for name, value in env.items():
        monkeypatch.setenv(name, value)
    monkeypatch.setattr(oauth.shutil, "which", lambda name: xdg)
    assert oauth.can_open_browser() is expected


# --- the Antigravity (Google) sign-in ----------------------------------------------------


def _google_tokens(*, refresh: str | None = "g-r1", email: str = "me@gmail.com") -> dict:
    tokens = {"access_token": "ya29.opaque", "expires_in": 3599, "token_type": "Bearer",
              "id_token": _jwt({"email": email})}
    if refresh:
        tokens["refresh_token"] = refresh
    return tokens


_LOAD_CODE_ASSIST = {
    "cloudaicompanionProject": "proj-123",
    "currentTier": {"id": "free-tier", "name": "Antigravity"},
    "paidTier": {"id": "g1-pro-tier", "name": "Google AI Pro"},
}


def _google(load_status: int = 200):
    def handle(request: httpx.Request) -> httpx.Response:
        if request.url.host == "oauth2.googleapis.com":
            return httpx.Response(200, json=_google_tokens())
        if request.url.path.endswith(":loadCodeAssist"):
            return httpx.Response(load_status, json=_LOAD_CODE_ASSIST)
        raise AssertionError(f"unexpected request {request.url}")

    return handle


# Assembled from pieces, so nothing in this file has the shape of a Google
# client for a secret scanner to flag.
_G_ID = "123456789012-" + "a" * 32 + ".apps.googleusercontent" + ".com"
_G_SECRET = "GOC" + "SPX-" + "b" * 28


@pytest.fixture
def google_client(monkeypatch):
    """Antigravity's client as the operator would set it."""
    monkeypatch.setenv("SEARCH_MCP_ANTIGRAVITY_CLIENT_ID", _G_ID)
    monkeypatch.setenv("SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET", _G_SECRET)


@pytest.fixture
def google_port(monkeypatch, google_client):
    """Point the Antigravity provider at a free port; 51121 may be taken."""
    (port,) = _free_ports(1)
    spec = oauth.OAuthProvider(**{**oauth.ANTIGRAVITY.__dict__, "redirect_ports": (port,)})
    monkeypatch.setitem(oauth.PROVIDERS, "antigravity", spec)
    return port


def test_the_antigravity_link_asks_google_for_a_refresh_token(google_client):
    spec = oauth._with_client(oauth.ANTIGRAVITY)
    link = urlparse(spec.authorize_link("CHALLENGE", "STATE", spec.redirect_uri(51121)))
    query = {k: v[0] for k, v in parse_qs(link.query).items()}
    assert f"{link.scheme}://{link.netloc}{link.path}" == (
        "https://accounts.google.com/o/oauth2/v2/auth"
    )
    assert query["client_id"] == _G_ID
    assert query["redirect_uri"] == "http://localhost:51121/oauth-callback"
    assert query["access_type"] == "offline" and query["prompt"] == "consent"
    assert "https://www.googleapis.com/auth/cloud-platform" in query["scope"].split()
    assert query["code_challenge_method"] == "S256"
    assert "client_secret" not in query


async def test_an_antigravity_sign_in_stores_the_account_project_and_tier(mock_http,
                                                                         google_port):
    state, calls = mock_http
    state["handler"] = _google()
    on_url, seen = _browser(lambda s: f"/oauth-callback?code=G-CODE&state={s}")

    await oauth.login("antigravity", open_browser=False, on_url=on_url, wait_seconds=5)
    await on_url.closed()

    assert seen[1][0].endswith("200 OK") and "Signed in" in seen[1][1]
    exchange, load = calls
    assert exchange.headers["content-type"] == "application/x-www-form-urlencoded"
    body = _body(exchange)
    assert body["code"] == "G-CODE"
    assert (body["client_id"], body["client_secret"]) == (_G_ID, _G_SECRET)
    assert body["redirect_uri"] == f"http://localhost:{google_port}/oauth-callback"
    assert load.headers["user-agent"].startswith("antigravity/")
    assert load.headers["authorization"] == "Bearer ya29.opaque"
    stored = oauth.load("antigravity")
    assert (stored.email, stored.account_id, stored.plan) == (
        "me@gmail.com", "proj-123", "Google AI Pro"
    )
    assert stored.refresh_token == "g-r1" and stored.expires_at > time.time() + 3000
    # The refresh token belongs to this client, so the sign-in keeps it.
    assert (stored.client_id, stored.client_secret) == (_G_ID, _G_SECRET)
    shown = json.dumps(oauth.status("antigravity"))
    assert "ya29" not in shown and _G_SECRET not in shown


async def test_a_failed_project_lookup_still_signs_in(mock_http, google_port):
    state, _ = mock_http
    state["handler"] = _google(load_status=500)
    on_url, _ = _browser(lambda s: f"/oauth-callback?code=G&state={s}")

    cred = await oauth.login("antigravity", open_browser=False, on_url=on_url, wait_seconds=5)
    await on_url.closed()

    assert cred.email == "me@gmail.com" and cred.account_id == "" and cred.plan == ""
    assert oauth.is_signed_in("antigravity")


def _ipv6_loopback() -> bool:
    try:
        with socket.socket(socket.AF_INET6) as sock:
            sock.bind(("::1", 0))
        return True
    except OSError:
        return False


@pytest.mark.skipif(not _ipv6_loopback(), reason="no IPv6 loopback here")
async def test_a_browser_that_reaches_localhost_over_ipv6_is_answered(mock_http, google_port):
    state, _ = mock_http
    state["handler"] = _google()
    link: asyncio.Future[str] = asyncio.get_running_loop().create_future()
    task = asyncio.create_task(oauth.login("antigravity", open_browser=False,
                                           on_url=link.set_result, wait_seconds=5))
    returned = parse_qs(urlparse(await link).query)["state"][0]

    reader, writer = await asyncio.open_connection("::1", google_port)
    writer.write(f"GET /oauth-callback?code=C&state={returned} HTTP/1.1\r\n"
                 "Host: localhost\r\n\r\n".encode())
    await writer.drain()
    status = (await reader.read()).decode().split("\r\n", 1)[0]
    writer.close()

    await task
    assert status.endswith("200 OK")


async def test_a_google_refresh_is_a_form_with_the_secret_and_keeps_the_token(mock_http,
                                                                             google_client):
    state, calls = mock_http
    state["handler"] = lambda request: httpx.Response(200, json=_google_tokens(refresh=None))
    oauth.save(oauth.Credential(provider="antigravity", access_token="old", refresh_token="g-r1",
                                expires_at=time.time() + 10, email="me@gmail.com",
                                account_id="proj-123", plan="Google AI Pro",
                                client_id="signed-in-id", client_secret="signed-in-secret"))

    cred = await oauth.credential("antigravity")

    (sent,) = calls
    assert str(sent.url) == "https://oauth2.googleapis.com/token"
    assert sent.headers["content-type"] == "application/x-www-form-urlencoded"
    # The client the token was issued to, not the one set since.
    assert _body(sent) == {
        "client_id": "signed-in-id",
        "client_secret": "signed-in-secret",
        "grant_type": "refresh_token",
        "refresh_token": "g-r1",
    }
    assert cred.access_token == "ya29.opaque"
    stored = oauth.load("antigravity")
    # Google sends no new refresh token, and the project outlives the refresh.
    assert (stored.refresh_token, stored.account_id, stored.plan) == (
        "g-r1", "proj-123", "Google AI Pro"
    )


async def test_a_sign_in_stored_without_its_client_is_given_one_on_refresh(mock_http,
                                                                          google_client):
    state, calls = mock_http
    state["handler"] = lambda request: httpx.Response(200, json=_google_tokens(refresh=None))
    oauth.save(oauth.Credential(provider="antigravity", access_token="old", refresh_token="g-r1",
                                expires_at=time.time() + 10))

    await oauth.credential("antigravity")

    (sent,) = calls
    assert (_body(sent)["client_id"], _body(sent)["client_secret"]) == (_G_ID, _G_SECRET)
    stored = oauth.load("antigravity")
    assert (stored.client_id, stored.client_secret) == (_G_ID, _G_SECRET)


# --- where Antigravity's client comes from ------------------------------------------------

_OTHER_ID = "987654321098-" + "c" * 32 + ".apps.googleusercontent" + ".com"
_OTHER_SECRET = "GOC" + "SPX-" + "d" * 28


def _sha(value: str) -> str:
    return hashlib.sha256(value.encode()).hexdigest()


@pytest.fixture
def install(tmp_path, monkeypatch):
    """A binary laid out like Antigravity's: two Google clients, the IDs apart
    from the secrets, the secrets back to back."""
    binary = tmp_path / "language_server"
    # Strings sit back to back there, so digits may run into an ID.
    binary.write_bytes(
        b"\x00" * 4096 + _OTHER_ID.encode() + b"\x00proto3.21.12" + _G_ID.encode()
        + b"\x00" * 4096 + _G_SECRET.encode() + _OTHER_SECRET.encode() + b"\x00" * 64
    )
    monkeypatch.setattr(oauth, "antigravity_install_paths", lambda: [binary])
    monkeypatch.setattr(oauth, "_ANTIGRAVITY_CLIENT_SHA256", (_sha(_G_ID), _sha(_G_SECRET)))
    return binary


def test_the_client_is_read_from_the_antigravity_install(install):
    assert oauth.antigravity_client() == (_G_ID, _G_SECRET)


def test_the_client_on_the_settings_page_wins_over_the_install(install):
    from search_mcp import keystore

    keystore.set_secrets({"antigravity_client_id": "set-id",
                          "antigravity_client_secret": "set-secret"})
    assert oauth.antigravity_client() == ("set-id", "set-secret")


def test_half_a_client_is_refused(install, monkeypatch):
    monkeypatch.setenv("SEARCH_MCP_ANTIGRAVITY_CLIENT_ID", _G_ID)
    with pytest.raises(oauth.OAuthError, match="only one of"):
        oauth.antigravity_client()


def test_an_install_without_the_known_client_is_named(install, monkeypatch):
    monkeypatch.setattr(oauth, "_ANTIGRAVITY_CLIENT_SHA256", ("0" * 64, "0" * 64))
    with pytest.raises(oauth.OAuthError) as caught:
        oauth.antigravity_client()
    message = str(caught.value)
    assert str(install) in message and "may have changed it" in message
    assert "SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET" in message


def test_an_install_that_cannot_be_read_is_not_called_a_new_version(install, tmp_path,
                                                                    monkeypatch):
    folder = tmp_path / "language_server_x"
    folder.mkdir()
    monkeypatch.setattr(oauth, "antigravity_install_paths", lambda: [folder])
    with pytest.raises(oauth.OAuthError, match="could not be read") as caught:
        oauth.antigravity_client()
    assert str(folder) in str(caught.value) and "changed" not in str(caught.value)


async def test_without_a_client_the_sign_in_stops_before_its_page(monkeypatch):
    opened: list[str] = []
    with pytest.raises(oauth.OAuthError) as caught:
        await oauth.login("antigravity", open_browser=False, on_url=opened.append,
                          wait_seconds=5)
    message = str(caught.value)
    assert "no Antigravity install was found" in message
    assert "SEARCH_MCP_ANTIGRAVITY_CLIENT_ID" in message
    assert opened == []


def test_the_repository_carries_no_google_client():
    root = Path(__file__).resolve().parents[1]
    listed = subprocess.run(["git", "ls-files", "-z"], cwd=root, capture_output=True,
                            check=False)
    if listed.returncode != 0:
        pytest.skip("not a git checkout")
    names = [name for name in listed.stdout.decode().split("\0") if name]
    assert "src/search_mcp/oauth.py" in names
    for path in (root / name for name in names):
        if not path.is_file():
            continue
        data = path.read_bytes()
        assert not oauth._GOOGLE_CLIENT_ID.search(data), path
        assert not oauth._GOOGLE_CLIENT_SECRET.search(data), path


@pytest.mark.parametrize(
    ("platform", "machine", "agent", "metadata"),
    [
        ("darwin", "arm64", "darwin/arm64", "DARWIN_ARM64"),
        ("linux", "x86_64", "linux/amd64", "LINUX_AMD64"),
        ("linux", "aarch64", "linux/arm64", "LINUX_ARM64"),
        ("win32", "AMD64", "windows/amd64", "WINDOWS_AMD64"),
    ],
)
def test_the_antigravity_headers_name_this_platform(monkeypatch, platform, machine, agent,
                                                    metadata):
    import platform as platform_module

    monkeypatch.setattr(oauth.sys, "platform", platform)
    monkeypatch.setattr(platform_module, "machine", lambda: machine)
    headers = oauth.antigravity_headers("TOKEN")
    assert headers["User-Agent"] == f"antigravity/{oauth.settings.antigravity_version} {agent}"
    assert json.loads(headers["Client-Metadata"]) == {
        "ideType": "ANTIGRAVITY", "platform": metadata, "pluginType": "GEMINI",
    }
    assert headers["Authorization"] == "Bearer TOKEN"
