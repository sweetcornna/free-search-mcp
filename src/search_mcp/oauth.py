"""Sign-in for the opt-in engine that searches on the operator's own account.

`codex` runs OpenAI's web search through the ChatGPT sign-in that the Codex CLI
uses. OpenAI supports that sign-in in third-party tools (OpenClaw uses the same
client), so a ChatGPT plan can pay for these searches instead of API credits.
It is not an API key: the operator signs in once in a browser
(`search-mcp-login codex`, or the button on the `search-mcp-admin` page), and
this module keeps the tokens fresh from then on.

Like the API-key engines, it is opt-in. No pool, reserve or category route
contains it (tests/test_no_key_positioning.py), so a search only reaches it
when a call names it, and every search spends the operator's own plan quota.

The flow is the one the Codex CLI uses: OAuth 2.0 authorization code with
PKCE, redirected to a listener on a fixed loopback port. The port is part of
the redirect URI the client registered, so it cannot be changed. When the
browser runs on another machine (SSH), the redirect fails to load, and the
address bar still holds the code: `search-mcp-login` reads that URL when it is
pasted into the terminal.

Google's Antigravity sign-in is deliberately absent. Its terms call any
third-party use of Antigravity OAuth a breach, and Google suspends accounts
for it.

Tokens live in ``<config_dir>/oauth/<provider>.json``, written atomically with
``0600`` permissions, and never reach a log line or a tool result. A refresh
runs under a per-process lock and, where the platform has ``fcntl``, a file
lock, because OpenAI rotates refresh tokens: two servers refreshing the same
token at once would leave one of them holding a revoked token.

A machine that already has the Codex CLI signed in can link to its
``auth.json`` instead (``search-mcp-login codex --use-codex-cli``). The link is
read-only: this server never refreshes or rewrites Codex's file, because a
rotation here would sign the CLI out. When the linked token expires, running
`codex` once refreshes it.
"""

from __future__ import annotations

import asyncio
import base64
import contextlib
import hashlib
import inspect
import json
import os
import secrets
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import uuid
import weakref
from collections.abc import AsyncIterator, Callable
from dataclasses import asdict, dataclass, field, fields
from pathlib import Path
from typing import Any
from urllib.parse import parse_qs, quote, urlencode, urlparse

import httpx

from .keystore import config_dir
from .net import proxy_for

# --- errors -------------------------------------------------------------------


class OAuthError(RuntimeError):
    """Signing in, or keeping a sign-in fresh, failed. The message says what to do."""


class NotSignedIn(OAuthError):
    """No sign-in is stored for this provider."""


# --- providers ----------------------------------------------------------------


@dataclass(frozen=True)
class OAuthProvider:
    id: str
    label: str
    engine: str
    authorize_url: str
    token_url: str
    client_id: str
    scopes: tuple[str, ...]
    # Tried in order. Each is on the client's registered redirect allow-list,
    # so no other port can be used.
    redirect_ports: tuple[int, ...]
    redirect_path: str
    extra_authorize_params: tuple[tuple[str, str], ...] = ()
    # What the account is called in messages.
    account: str = ""

    def redirect_uri(self, port: int) -> str:
        return f"http://127.0.0.1:{port}{self.redirect_path}"

    def authorize_link(self, challenge: str, state: str, redirect_uri: str) -> str:
        params = {
            "response_type": "code",
            "client_id": self.client_id,
            "redirect_uri": redirect_uri,
            "scope": " ".join(self.scopes),
            "code_challenge": challenge,
            "code_challenge_method": "S256",
            "state": state,
            **dict(self.extra_authorize_params),
        }
        return f"{self.authorize_url}?{urlencode(params, quote_via=quote)}"


# The Codex CLI's public client (codex-rs/login: CLIENT_ID, DEFAULT_PORT and
# FALLBACK_PORT, checked 2026-09-26). The simplified-flow and originator
# parameters are what the CLI sends, so the consent page is the one `codex
# login` shows. The CLI also asks for two connector scopes; a search needs
# neither, so they are left out.
CODEX = OAuthProvider(
    id="codex",
    label="Codex (ChatGPT sign-in)",
    engine="codex",
    authorize_url="https://auth.openai.com/oauth/authorize",
    token_url="https://auth.openai.com/oauth/token",
    client_id="app_EMoamEEZ73f0CkXaXp7hrann",
    scopes=("openid", "profile", "email", "offline_access"),
    redirect_ports=(1455, 1457),
    redirect_path="/auth/callback",
    extra_authorize_params=(
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", "codex_cli_rs"),
    ),
    account="ChatGPT",
)


PROVIDERS: dict[str, OAuthProvider] = {p.id: p for p in (CODEX,)}


def provider(provider_id: str) -> OAuthProvider:
    try:
        return PROVIDERS[provider_id]
    except KeyError:
        raise ValueError(
            f"unknown sign-in provider {provider_id!r}: use {', '.join(PROVIDERS)}"
        ) from None


# --- stored credential --------------------------------------------------------


@dataclass
class Credential:
    provider: str
    access_token: str = ""
    refresh_token: str = ""
    # Epoch seconds. 0 means unknown, which is treated as expired.
    expires_at: float = 0.0
    id_token: str = ""
    email: str = ""
    # The ChatGPT workspace the requests are billed to, and its plan.
    account_id: str = ""
    plan: str = ""
    # "login" (this server's own token chain) or "codex-cli" (a read-only link
    # to the Codex CLI's auth.json, whose path is `linked_path`).
    source: str = "login"
    linked_path: str = ""
    updated_at: float = field(default_factory=time.time)

    def expiring(self, margin: float = 300.0) -> bool:
        return not self.access_token or self.expires_at <= time.time() + margin

    def public(self) -> dict[str, Any]:
        """What a status line may show. No token, ever."""
        return {
            "provider": self.provider,
            "email": self.email,
            "plan": self.plan,
            "source": self.source,
            "expires_at": self.expires_at,
        }


def oauth_dir() -> Path:
    return config_dir() / "oauth"


def credential_path(provider_id: str) -> Path:
    return oauth_dir() / f"{provider(provider_id).id}.json"


def codex_cli_auth_path() -> Path:
    """Where the Codex CLI keeps its sign-in: ``$CODEX_HOME/auth.json``."""
    home = os.environ.get("CODEX_HOME")
    base = Path(home).expanduser() if home else Path.home() / ".codex"
    return base / "auth.json"


def _write_private(path: Path, payload: dict[str, Any]) -> None:
    directory = path.parent
    directory.mkdir(parents=True, exist_ok=True)
    with contextlib.suppress(OSError):
        directory.chmod(0o700)
    fd, tmp = tempfile.mkstemp(dir=str(directory), prefix=f".{path.stem}.", suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            json.dump(payload, fh, ensure_ascii=False, indent=2)
        os.chmod(tmp, 0o600)
        os.replace(tmp, path)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)


def save(cred: Credential) -> None:
    cred.updated_at = time.time()
    payload = asdict(cred)
    if cred.source == "codex-cli":
        # A link stores where the tokens are, not the tokens.
        payload = {
            "provider": cred.provider,
            "source": cred.source,
            "linked_path": cred.linked_path,
            "updated_at": cred.updated_at,
        }
    _write_private(credential_path(cred.provider), payload)


def _from_dict(data: dict[str, Any]) -> Credential:
    known = {f.name for f in fields(Credential)}
    values = {k: v for k, v in data.items() if k in known}
    values["expires_at"] = float(values.get("expires_at") or 0.0)
    for name in known - {"expires_at", "updated_at"}:
        if name in values and not isinstance(values[name], str):
            values[name] = str(values[name])
    return Credential(**values)


def _read_codex_cli(cred: Credential) -> Credential:
    """Fill a link record with the tokens currently in the Codex CLI's file."""
    path = Path(cred.linked_path or codex_cli_auth_path())
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return cred
    tokens = data.get("tokens") if isinstance(data, dict) else None
    # An API-key or other non-ChatGPT login has no ChatGPT tokens to lend.
    mode = data.get("auth_mode") if isinstance(data, dict) else None
    if not isinstance(tokens, dict) or mode not in (None, "chatgpt", "chatgptAuthTokens"):
        return cred
    cred.access_token = str(tokens.get("access_token") or "")
    cred.id_token = str(tokens.get("id_token") or "")
    cred.refresh_token = ""  # never used from here: see the module docstring
    cred.account_id = str(tokens.get("account_id") or "")
    _apply_codex_claims(cred)
    return cred


def load(provider_id: str) -> Credential | None:
    """The stored sign-in, or None. Never raises on a missing or corrupt file."""
    try:
        data = json.loads(credential_path(provider_id).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    if not isinstance(data, dict):
        return None
    try:
        cred = _from_dict({**data, "provider": provider(provider_id).id})
    except (TypeError, ValueError):
        return None
    if cred.source == "codex-cli":
        cred = _read_codex_cli(cred)
        return cred if cred.access_token else None
    return cred if (cred.access_token or cred.refresh_token) else None


def is_signed_in(provider_id: str) -> bool:
    return load(provider_id) is not None


def sign_out(provider_id: str) -> bool:
    """Forget the stored sign-in. A linked Codex CLI login itself is untouched."""
    path = credential_path(provider_id)
    try:
        path.unlink()
    except FileNotFoundError:
        return False
    return True


def status(provider_id: str) -> dict[str, Any]:
    spec = provider(provider_id)
    cred = load(provider_id)
    out: dict[str, Any] = {
        "provider": spec.id,
        "label": spec.label,
        "engine": spec.engine,
        "signed_in": cred is not None,
    }
    if cred is not None:
        out.update(cred.public())
        out["expired"] = cred.expiring(margin=0) and not cred.refresh_token
    return out


# --- tokens ---------------------------------------------------------------------


def jwt_claims(token: str) -> dict[str, Any]:
    """The payload of a JWT, unverified. Only read from tokens the issuer sent us
    over TLS, to learn an expiry or an account id; never used to authorize."""
    try:
        payload = token.split(".")[1]
        payload += "=" * (-len(payload) % 4)
        data = json.loads(base64.urlsafe_b64decode(payload.encode()))
    except (IndexError, ValueError):
        return {}
    return data if isinstance(data, dict) else {}


def _claim(claims: dict[str, Any], namespace: str) -> dict[str, Any]:
    value = claims.get(f"https://api.openai.com/{namespace}")
    return value if isinstance(value, dict) else {}


def _apply_codex_claims(cred: Credential) -> None:
    claims = jwt_claims(cred.id_token)
    auth = _claim(claims, "auth")
    orgs = claims.get("organizations")
    first_org = orgs[0] if isinstance(orgs, list) and orgs and isinstance(orgs[0], dict) else {}
    cred.email = str(
        claims.get("email") or _claim(claims, "profile").get("email") or cred.email or ""
    )
    cred.account_id = str(
        auth.get("chatgpt_account_id")
        or claims.get("chatgpt_account_id")
        or first_org.get("id")
        or cred.account_id
        or ""
    )
    cred.plan = str(auth.get("chatgpt_plan_type") or cred.plan or "")
    exp = jwt_claims(cred.access_token).get("exp")
    if isinstance(exp, (int, float)):
        cred.expires_at = float(exp)


def pkce_pair() -> tuple[str, str]:
    """`(verifier, S256 challenge)` per RFC 7636."""
    verifier = base64.urlsafe_b64encode(secrets.token_bytes(64)).rstrip(b"=").decode()
    digest = hashlib.sha256(verifier.encode()).digest()
    return verifier, base64.urlsafe_b64encode(digest).rstrip(b"=").decode()


def http_client(engine: str | None = None, *, timeout: float = 30.0) -> httpx.AsyncClient:
    """An httpx client that honours ``SEARCH_MCP_PROXY`` for this engine.

    The sign-in host (auth.openai.com) is walled off exactly where a search
    proxy is needed, so it goes through it too.
    """
    kwargs: dict[str, Any] = {"timeout": httpx.Timeout(timeout, connect=15.0)}
    proxy = proxy_for(engine)
    if proxy:
        kwargs["proxy"] = proxy
    return httpx.AsyncClient(**kwargs)


def _brief_body(response: httpx.Response) -> str:
    text = response.text.strip()
    try:
        data = response.json()
    except ValueError:
        return text[:200]
    if isinstance(data, dict):
        err = data.get("error")
        if isinstance(err, dict):
            return str(err.get("message") or err.get("code") or err)[:200]
        desc = data.get("error_description")
        if err or desc:
            return f"{err or ''}{': ' if err and desc else ''}{desc or ''}"[:200]
    return text[:200]


async def _token_request(
    client: httpx.AsyncClient, spec: OAuthProvider, form: dict[str, str]
) -> dict[str, Any]:
    body = {"client_id": spec.client_id, **form}
    # The CLI exchanges a code form-encoded and refreshes with a JSON body.
    encoding: dict[str, Any] = (
        {"json": body} if form.get("grant_type") == "refresh_token" else {"data": body}
    )
    try:
        response = await client.post(
            spec.token_url, headers={"Accept": "application/json"}, **encoding
        )
    except httpx.HTTPError as exc:
        raise OAuthError(
            f"could not reach {urlparse(spec.token_url).netloc}: {type(exc).__name__}. "
            "If it is blocked on this network, set SEARCH_MCP_PROXY."
        ) from exc
    if response.status_code >= 400:
        detail = _brief_body(response)
        if form.get("grant_type") == "refresh_token" and response.status_code in (400, 401):
            raise OAuthError(
                f"the {spec.account} sign-in has expired or was revoked ({detail}). "
                f"Sign in again with `search-mcp-login {spec.id}`."
            )
        raise OAuthError(f"{spec.account} token endpoint answered HTTP {response.status_code}: "
                         f"{detail}")
    try:
        data = response.json()
    except ValueError as exc:
        raise OAuthError(f"{spec.account} token endpoint did not return JSON") from exc
    if not isinstance(data, dict) or not data.get("access_token"):
        raise OAuthError(f"{spec.account} token endpoint returned no access token")
    return data


def _expiry(data: dict[str, Any], access_token: str) -> float:
    exp = jwt_claims(access_token).get("exp")
    if isinstance(exp, (int, float)):
        return float(exp)
    try:
        return time.time() + float(data.get("expires_in") or 3600)
    except (TypeError, ValueError):
        return time.time() + 3600


def _credential_from_tokens(
    spec: OAuthProvider, data: dict[str, Any], previous: Credential | None = None
) -> Credential:
    cred = Credential(provider=spec.id) if previous is None else previous
    cred.access_token = str(data["access_token"])
    # OpenAI rotates the refresh token on every refresh; a reply without one
    # keeps the old.
    cred.refresh_token = str(data.get("refresh_token") or cred.refresh_token or "")
    cred.id_token = str(data.get("id_token") or cred.id_token or "")
    cred.expires_at = _expiry(data, cred.access_token)
    cred.source = "login"
    _apply_codex_claims(cred)
    return cred


def _check_account(cred: Credential) -> None:
    """What the Codex backend needs beyond the tokens before a search can run."""
    if not cred.account_id:
        raise OAuthError(
            "the ChatGPT sign-in carried no workspace id, so the Codex backend would "
            "refuse it. Sign in with an account that has a ChatGPT plan."
        )


# --- refresh ---------------------------------------------------------------------

_locks: weakref.WeakKeyDictionary[asyncio.AbstractEventLoop, dict[str, asyncio.Lock]] = (
    weakref.WeakKeyDictionary()
)


def _lock(provider_id: str) -> asyncio.Lock:
    per_loop = _locks.setdefault(asyncio.get_running_loop(), {})
    return per_loop.setdefault(provider_id, asyncio.Lock())


@contextlib.asynccontextmanager
async def _file_lock(provider_id: str, wait: float = 30.0) -> AsyncIterator[None]:
    """Hold ``<oauth_dir>/.<provider>.lock`` across processes, where fcntl exists."""
    try:
        import fcntl
    except ImportError:  # Windows: the per-process lock is all there is
        yield
        return
    oauth_dir().mkdir(parents=True, exist_ok=True)
    fd = os.open(oauth_dir() / f".{provider_id}.lock", os.O_RDWR | os.O_CREAT, 0o600)
    try:
        deadline = time.monotonic() + wait
        while True:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except OSError:
                if time.monotonic() > deadline:
                    raise OAuthError(
                        "another process has been refreshing this sign-in for "
                        f"{wait:g} s; try again"
                    ) from None
                await asyncio.sleep(0.1)
        try:
            yield
        finally:
            with contextlib.suppress(OSError):
                fcntl.flock(fd, fcntl.LOCK_UN)
    finally:
        os.close(fd)


async def credential(provider_id: str, *, force_refresh: bool = False) -> Credential:
    """A signed-in credential whose access token is good for a few minutes.

    Raises `NotSignedIn` when there is nothing stored, and `OAuthError` when the
    stored sign-in cannot be refreshed.
    """
    spec = provider(provider_id)
    cred = load(spec.id)
    if cred is None:
        raise NotSignedIn(f"{spec.id}: not signed in")
    if not force_refresh and not cred.expiring():
        return cred
    async with _lock(spec.id), _file_lock(spec.id):
        # Another task or process may have refreshed while this one waited.
        cred = load(spec.id)
        if cred is None:
            raise NotSignedIn(f"{spec.id}: not signed in")
        if cred.source == "codex-cli":
            # Read-only: the CLI owns this token chain (see the module docstring).
            if force_refresh or cred.expiring(margin=0):
                state = "was rejected" if force_refresh else "has expired"
                raise OAuthError(
                    f"the linked Codex CLI sign-in at {cred.linked_path} {state}. Run "
                    "`codex` once to refresh it, or give this server its own sign-in with "
                    "`search-mcp-login codex`."
                )
            return cred
        if not force_refresh and not cred.expiring():
            return cred
        if not cred.refresh_token:
            raise OAuthError(
                f"the {spec.account} sign-in has expired and holds no refresh token. "
                f"Sign in again with `search-mcp-login {spec.id}`."
            )
        form = {"grant_type": "refresh_token", "refresh_token": cred.refresh_token}
        async with http_client(spec.engine) as client:
            data = await _token_request(client, spec, form)
        cred = _credential_from_tokens(spec, data, cred)
        save(cred)
        return cred


# --- the browser sign-in -------------------------------------------------------------


_PAGE = (
    "<!doctype html><html><head><meta charset=\"utf-8\"><title>free-search-mcp</title></head>"
    "<body style=\"font:16px/1.5 system-ui,sans-serif;margin:3rem auto;max-width:36rem\">"
    "<h2>{title}</h2><p>{body}</p></body></html>"
)


# How long the browser's tab waits for the token exchange before it answers.
_REPLY_WAIT = 60.0


def _page(ok: bool | None, detail: str = "") -> bytes:
    import html

    if ok:
        title = "Signed in / 登录成功"
        body = ("You can close this tab and go back to the terminal or the settings page. "
                "可以关闭此页面，回到终端或设置页。")
    elif ok is None:
        title = "Still finishing / 仍在完成登录"
        body = ("The sign-in was received but is taking long to complete. The terminal or the "
                "settings page shows whether it succeeded. 已收到授权，但完成得较慢，"
                "请在终端或设置页查看是否成功。")
    else:
        title = "Sign-in failed / 登录失败"
        body = html.escape(detail or "unknown error")
    return _PAGE.format(title=title, body=body).encode()


def state_matches(returned: str, expected: str) -> bool:
    # ChatGPT may append an onboarding suffix to the state it returns. A prefix
    # match keeps the check: the random part is still unguessable.
    return bool(expected) and returned.startswith(expected)


def read_redirect(params: dict[str, list[str]], state: str) -> str:
    """The authorization code from a redirect's query, after checking `state`."""
    def one(name: str) -> str:
        values = params.get(name) or [""]
        return values[0]

    if one("error"):
        detail = one("error_description")
        if "missing_codex_entitlement" in detail:
            raise OAuthError(
                "this ChatGPT workspace does not include Codex, so it cannot be used for "
                "searches. Sign in with a plan that includes Codex, or ask the workspace admin "
                "to enable it."
            )
        raise OAuthError(f"the sign-in was not completed: {one('error')}"
                         + (f" ({detail})" if detail else ""))
    if not state_matches(one("state"), state):
        raise OAuthError("the sign-in reply did not match this attempt (state mismatch); "
                         "start the sign-in again")
    code = one("code")
    if not code:
        raise OAuthError("the sign-in reply carried no authorization code")
    return code


def parse_pasted(text: str, state: str) -> str:
    """A pasted redirect URL (or bare query) -> the authorization code."""
    text = text.strip()
    if "code=" not in text and "error=" not in text:
        raise OAuthError(
            "paste the whole address of the page the browser landed on; it contains `code=`"
        )
    query = urlparse(text).query if "://" in text else text.lstrip("?")
    return read_redirect(parse_qs(query), state)


async def _serve_callback(
    spec: OAuthProvider, state: str, outcome: asyncio.Future[str], finished: asyncio.Future[str]
) -> tuple[asyncio.AbstractServer, int]:
    """Listen for the redirect. `outcome` gets the code; the browser's page
    then waits on `finished` ("" once the tokens are stored, else the error),
    so the tab says "Signed in" only when that is true."""
    async def handle(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        status, page = "404 Not Found", b"not found"
        try:
            line = await asyncio.wait_for(reader.readline(), 10)
            while True:  # headers; nothing in them matters here
                header = await asyncio.wait_for(reader.readline(), 10)
                if header in (b"\r\n", b"\n", b""):
                    break
            parts = line.decode("latin-1").split()
            target = urlparse(parts[1]) if len(parts) >= 2 else None
            if target is not None and target.path == spec.redirect_path:
                params = parse_qs(target.query)
                if not state_matches((params.get("state") or [""])[0], state):
                    # Not this sign-in's reply. Any web page can make the
                    # browser request this address, so a stray one is refused
                    # and the listener keeps waiting for the real one, as the
                    # Codex CLI's does.
                    status, page = "400 Bad Request", _page(
                        False, "This reply does not belong to the sign-in in progress, so it "
                        "was ignored. / 这个回调不属于当前的登录，已忽略。"
                    )
                else:
                    try:
                        code = read_redirect(params, state)
                    except OAuthError as exc:
                        status, page = "400 Bad Request", _page(False, str(exc))
                        if not outcome.done():
                            outcome.set_exception(exc)
                    else:
                        if not outcome.done():
                            outcome.set_result(code)
                        try:
                            problem = await asyncio.wait_for(
                                asyncio.shield(finished), _REPLY_WAIT
                            )
                        except TimeoutError:
                            # Not known yet, so neither success nor failure.
                            status, page = "202 Accepted", _page(None)
                        else:
                            if problem:
                                status, page = "400 Bad Request", _page(False, problem)
                            else:
                                status, page = "200 OK", _page(True)
        except (TimeoutError, ConnectionError, UnicodeError):
            return
        finally:
            with contextlib.suppress(Exception):
                writer.write(
                    f"HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n"
                    f"Content-Length: {len(page)}\r\nConnection: close\r\n\r\n".encode()
                    + page
                )
                await writer.drain()
            writer.close()

    last: OSError | None = None
    for port in spec.redirect_ports:
        try:
            return await asyncio.start_server(handle, "127.0.0.1", port), port
        except OSError as exc:
            last = exc
    raise last or OSError("no redirect port to listen on")


def _read_stdin(outcome: asyncio.Future[str], state: str, loop: asyncio.AbstractEventLoop) -> None:
    """Feed pasted redirect URLs into `outcome` from a daemon thread.

    A daemon thread, not the loop's executor: `asyncio.run` waits for executor
    threads on exit, and this one is blocked in readline until the user types.
    """
    def settle(code: str | None, error: OAuthError | None) -> None:
        if outcome.done():
            return
        if error is not None:
            print(f"  {error}", file=sys.stderr)
            return
        outcome.set_result(code or "")

    def run() -> None:
        for line in sys.stdin:
            if outcome.done():
                return
            if not line.strip():
                continue
            try:
                code = parse_pasted(line, state)
            except OAuthError as exc:
                loop.call_soon_threadsafe(settle, None, exc)
                continue
            loop.call_soon_threadsafe(settle, code, None)
            return

    threading.Thread(target=run, name="oauth-paste", daemon=True).start()


async def login(
    provider_id: str,
    *,
    open_browser: bool = True,
    paste: bool = False,
    on_url: Callable[[str], Any] | None = None,
    wait_seconds: float = 600.0,
) -> Credential:
    """Run the browser sign-in and store the result.

    `on_url` receives the authorization link before anything waits on it (the
    CLI prints it, the settings page hands it to the browser). With `paste`,
    a redirect URL typed into stdin is accepted as well as the loopback
    callback, which is what makes a sign-in over SSH possible.
    """
    spec = provider(provider_id)
    verifier, challenge = pkce_pair()
    state = secrets.token_urlsafe(32)
    loop = asyncio.get_running_loop()
    outcome: asyncio.Future[str] = loop.create_future()
    finished: asyncio.Future[str] = loop.create_future()
    server: asyncio.AbstractServer | None = None
    port = spec.redirect_ports[0]
    try:
        server, port = await _serve_callback(spec, state, outcome, finished)
    except OSError as exc:
        # Over SSH the paste is the way back anyway, and it does not need the
        # listener; locally a sign-in cannot finish without one.
        if not paste:
            ports = " and ".join(str(p) for p in spec.redirect_ports)
            raise OAuthError(
                f"port {ports} on this machine are in use, and the {spec.account} sign-in can "
                f"only return to one of them. Close whatever holds them (a `codex login` or "
                f"another sign-in in progress?) and try again."
            ) from exc
    redirect_uri = spec.redirect_uri(port)
    link = spec.authorize_link(challenge, state, redirect_uri)
    try:
        if on_url is not None:
            result = on_url(link)
            if inspect.isawaitable(result):
                await result
        if open_browser:
            import webbrowser

            with contextlib.suppress(Exception):
                await asyncio.to_thread(webbrowser.open, link)
        if paste:
            _read_stdin(outcome, state, loop)
        try:
            code = await asyncio.wait_for(outcome, wait_seconds)
        except TimeoutError:
            raise OAuthError(f"no sign-in within {wait_seconds:g} s") from None
    finally:
        # Stops new connections; the browser's own request is still answered.
        if server is not None:
            server.close()
    try:
        async with http_client(spec.engine) as client:
            data = await _token_request(
                client,
                spec,
                {
                    "grant_type": "authorization_code",
                    "code": code,
                    "redirect_uri": redirect_uri,
                    "code_verifier": verifier,
                },
            )
        cred = _credential_from_tokens(spec, data)
        _check_account(cred)
        save(cred)
    except BaseException as exc:
        if not finished.done():
            finished.set_result(str(exc) or type(exc).__name__)
        raise
    finished.set_result("")
    return cred


# --- sign-in on first use -------------------------------------------------------------
# When an engine that needs a sign-in is named and none is stored, the MCP
# server can open the sign-in page itself instead of failing, and the search
# carries on once the operator approves it. The listener and the wait live in
# the server's own event loop, so a sign-in that outlasts one tool call is
# still there for the next.


class SignInPending(OAuthError):
    """The sign-in page is open in the browser and has not been approved yet."""


def can_open_browser() -> bool:
    """Whether a desktop browser can be started from this process."""
    if sys.platform in ("darwin", "win32"):
        return True
    has_display = bool(os.environ.get("DISPLAY") or os.environ.get("WAYLAND_DISPLAY"))
    return has_display and shutil.which("xdg-open") is not None


def open_in_browser(url: str) -> bool:
    """Start the desktop browser on `url` with every standard stream detached.

    Not `webbrowser.open`. Over stdio the MCP server speaks JSON-RPC on
    stdout, and a launched browser inherits it: whatever the browser prints
    (and a console browser such as lynx, which `webbrowser` falls back to on a
    Linux without a display, prints everything) would corrupt the stream.
    """
    try:
        if sys.platform == "win32":
            os.startfile(url)  # type: ignore[attr-defined]
            return True
        command = ["open", url] if sys.platform == "darwin" else ["xdg-open", url]
        subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
    except OSError:
        return False
    return True


@dataclass
class _FirstUse:
    task: asyncio.Task[Credential]
    # Set when the attempt ended without a sign-in. The page is not opened
    # again by itself after that, so an operator who closed it is not shown
    # it on every search; `search-mcp-login` still works at any time.
    failure: str = ""


_first_use: weakref.WeakKeyDictionary[asyncio.AbstractEventLoop, dict[str, _FirstUse]] = (
    weakref.WeakKeyDictionary()
)

# How long a first-use sign-in page stays answerable.
FIRST_USE_WINDOW = 600.0


async def sign_in_on_first_use(provider_id: str, wait_seconds: float) -> Credential:
    """Open the sign-in page and wait up to `wait_seconds` for its approval.

    Raises `SignInPending` while the page is open and unanswered (a later call
    waits on the same attempt; it does not open a second page), and
    `OAuthError` when no browser could be opened or an earlier automatic
    attempt ended without a sign-in.
    """
    spec = provider(provider_id)
    attempts = _first_use.setdefault(asyncio.get_running_loop(), {})
    attempt = attempts.get(spec.id)
    if attempt is not None and attempt.failure:
        raise OAuthError(attempt.failure)
    if attempt is None or attempt.task.done():

        async def launch(url: str) -> None:
            if not await asyncio.to_thread(open_in_browser, url):
                raise OAuthError("no browser could be opened on this machine")

        task = asyncio.get_running_loop().create_task(
            login(spec.id, open_browser=False, paste=False, on_url=launch,
                  wait_seconds=FIRST_USE_WINDOW)
        )
        attempt = attempts[spec.id] = _FirstUse(task=task)

        def settle(done: asyncio.Task[Credential], record: _FirstUse = attempt) -> None:
            if done.cancelled() or done.exception() is not None:
                reason = "cancelled" if done.cancelled() else str(done.exception())
                record.failure = (
                    f"the automatic {spec.account} sign-in did not complete ({reason}), so it "
                    f"is not opened again by itself"
                )

        task.add_done_callback(settle)
    try:
        # Shielded: a search that stops waiting must not end the sign-in.
        return await asyncio.wait_for(asyncio.shield(attempt.task), wait_seconds)
    except TimeoutError:
        raise SignInPending(
            f"the {spec.account} sign-in page is open in the browser and not yet approved"
        ) from None


def link_codex_cli(path: str | Path | None = None) -> Credential:
    """Point the `codex` engine at the Codex CLI's own sign-in, read-only."""
    target = Path(path).expanduser() if path else codex_cli_auth_path()
    probe = _read_codex_cli(Credential(provider="codex", source="codex-cli",
                                       linked_path=str(target)))
    if not probe.access_token:
        raise OAuthError(
            f"{target} holds no ChatGPT sign-in. Run `codex login` first (choose "
            "\"Sign in with ChatGPT\"), or sign in here with `search-mcp-login codex`."
        )
    if not probe.account_id:
        raise OAuthError(f"{target} has no ChatGPT workspace id; run `codex login` again")
    save(probe)
    return probe


def new_session_id() -> str:
    return str(uuid.uuid4())


__all__ = [
    "CODEX",
    "PROVIDERS",
    "Credential",
    "NotSignedIn",
    "OAuthError",
    "OAuthProvider",
    "SignInPending",
    "can_open_browser",
    "open_in_browser",
    "sign_in_on_first_use",
    "codex_cli_auth_path",
    "credential",
    "credential_path",
    "http_client",
    "is_signed_in",
    "jwt_claims",
    "link_codex_cli",
    "load",
    "login",
    "parse_pasted",
    "pkce_pair",
    "provider",
    "read_redirect",
    "save",
    "sign_out",
    "status",
]
