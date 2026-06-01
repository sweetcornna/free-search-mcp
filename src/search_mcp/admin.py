"""Local admin backend for configuring API-key search providers.

A tiny, single-page web app (Starlette + uvicorn, both pulled in by the ``mcp``
dependency — no extra deps, no template engine) that lets you paste provider API
keys and persist them via :mod:`search_mcp.keystore`.

Security posture (this tool writes secrets, so it stays deliberately small):
  * Binds ``127.0.0.1`` ONLY — never ``0.0.0.0``. It is a local config tool.
  * NEVER renders or echoes a stored secret value back to the page; the UI shows
    only a "Configured ✓ / Not configured" badge per provider.
  * NEVER logs secret values.
  * A blank input is dropped before saving, so submitting an empty field leaves
    an existing key untouched (it can't accidentally wipe a key).

Run with ``main()`` (the ``search-mcp-admin`` console script) or
``python -m search_mcp.admin``.
"""

from __future__ import annotations

import html
import os

from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import HTMLResponse, JSONResponse
from starlette.routing import Route

from . import keystore


# --- HTML rendering (no template engine; build the page as a string) --------


def _esc(text: str) -> str:
    """HTML-escape a value for safe inclusion in markup/attributes."""
    return html.escape(str(text), quote=True)


def _render_provider_card(provider: keystore.Provider) -> str:
    configured = keystore.is_configured(provider.id)
    badge_cls = "ok" if configured else "no"
    badge_txt = "Configured ✓" if configured else "Not configured"

    steps = "".join(f"<li>{_esc(step)}</li>" for step in provider.how_to)

    links = [f'<a href="{_esc(provider.signup_url)}" target="_blank" rel="noopener">Sign up</a>']
    if provider.docs_url:
        links.append(f'<a href="{_esc(provider.docs_url)}" target="_blank" rel="noopener">Docs</a>')
    links_html = " · ".join(links)

    inputs = []
    for field in provider.fields:
        ftype = "password" if field.secret else "text"
        # NEVER pre-fill the value — only ever render an empty input.
        inputs.append(
            f'<label class="field">'
            f'<span class="field-label">{_esc(field.label)}</span>'
            f'<input type="{ftype}" data-key="{_esc(field.key)}" '
            f'placeholder="{_esc(field.placeholder)}" autocomplete="off" '
            f'spellcheck="false" />'
            f"</label>"
        )
    inputs_html = "".join(inputs)

    # zhihu authenticates via an interactive browser login, not an API key.
    login_btn = ""
    if provider.id == "zhihu":
        login_btn = '<button class="login" onclick="loginProvider(this)">Login</button>'

    return f"""
    <section class="card" data-provider="{_esc(provider.id)}">
      <div class="card-head">
        <h2>{_esc(provider.label)}</h2>
        <span class="badge {badge_cls}" data-badge>{badge_txt}</span>
      </div>
      <p class="free-tier">{_esc(provider.free_tier)}</p>
      <details class="howto">
        <summary>How to get a key</summary>
        <ol>{steps}</ol>
        <p class="links">{links_html}</p>
      </details>
      <div class="fields">{inputs_html}</div>
      <div class="actions">
        <button class="save" onclick="saveProvider(this)">Save</button>
        <button class="test" onclick="testProvider(this)">Test</button>
        {login_btn}
        <button class="clear" onclick="clearProvider(this)">Clear</button>
        <span class="result" data-result></span>
      </div>
    </section>
    """


def _render_network_card() -> str:
    """The Network / Proxy card, built from ``keystore.NETWORK_FIELDS``.

    Uses the same masked-input + Save pattern as the provider cards: the proxy
    field is a secret -> password input, and the stored value is NEVER echoed
    (inputs are always rendered empty). ``data-key`` wires each input into the
    existing /api/save flow, so the two fields persist like any other secret."""
    inputs = []
    for field in keystore.NETWORK_FIELDS:
        ftype = "password" if field.secret else "text"
        # NEVER pre-fill the value — only ever render an empty input.
        inputs.append(
            f'<label class="field">'
            f'<span class="field-label">{_esc(field.label)}</span>'
            f'<input type="{ftype}" data-key="{_esc(field.key)}" '
            f'placeholder="{_esc(field.placeholder)}" autocomplete="off" '
            f'spellcheck="false" />'
            f"</label>"
        )
    inputs_html = "".join(inputs)
    configured = keystore.get_secret("proxy") is not None
    badge_cls = "ok" if configured else "no"
    badge_txt = "Configured ✓" if configured else "Not configured"

    return f"""
    <section class="card" data-provider="__network__">
      <div class="card-head">
        <h2>Network / Proxy</h2>
        <span class="badge {badge_cls}" data-badge>{badge_txt}</span>
      </div>
      <p class="free-tier">A proxy fixes datacenter-IP CAPTCHA gating.</p>
      <div class="fields">{inputs_html}</div>
      <div class="actions">
        <button class="save" onclick="saveProvider(this)">Save</button>
        <button class="clear" onclick="clearProvider(this)">Clear</button>
        <span class="result" data-result></span>
      </div>
    </section>
    """


_STYLE = """
:root { color-scheme: light dark; }
* { box-sizing: border-box; }
body {
  font: 15px/1.5 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
  margin: 0; padding: 2rem 1rem; background: #f5f6f8; color: #1a1d21;
}
.wrap { max-width: 720px; margin: 0 auto; }
header h1 { margin: 0 0 .25rem; font-size: 1.4rem; }
.note {
  margin: 0 0 1.5rem; padding: .6rem .8rem; background: #fff6e0;
  border: 1px solid #ecd9a0; border-radius: 8px; font-size: .85rem; color: #6a5300;
}
.card {
  background: #fff; border: 1px solid #e2e5ea; border-radius: 12px;
  padding: 1rem 1.1rem; margin-bottom: 1rem;
}
.card-head { display: flex; align-items: center; justify-content: space-between; gap: .5rem; }
.card-head h2 { margin: 0; font-size: 1.05rem; }
.badge { font-size: .75rem; padding: .15rem .55rem; border-radius: 999px; white-space: nowrap; }
.badge.ok { background: #e3f6e8; color: #15803d; border: 1px solid #aee0bd; }
.badge.no { background: #f0f1f3; color: #6b7280; border: 1px solid #d7dae0; }
.free-tier { margin: .35rem 0 .6rem; font-size: .85rem; color: #5a6270; }
.howto { margin-bottom: .7rem; font-size: .85rem; }
.howto summary { cursor: pointer; color: #2563eb; }
.howto ol { margin: .5rem 0; padding-left: 1.2rem; }
.howto li { margin: .25rem 0; }
.howto .links a { color: #2563eb; text-decoration: none; }
.fields { display: flex; flex-direction: column; gap: .5rem; }
.field { display: flex; flex-direction: column; gap: .2rem; }
.field-label { font-size: .8rem; color: #5a6270; }
.field input {
  padding: .5rem .6rem; border: 1px solid #cfd4dc; border-radius: 8px;
  font: inherit; background: #fcfcfd;
}
.field input:focus { outline: 2px solid #2563eb55; border-color: #2563eb; }
.actions { display: flex; align-items: center; gap: .5rem; margin-top: .8rem; }
button {
  font: inherit; padding: .45rem .9rem; border-radius: 8px; cursor: pointer; border: 1px solid transparent;
}
button.save { background: #2563eb; color: #fff; }
button.save:hover { background: #1d4ed8; }
button.test { background: #fff; color: #1a1d21; border-color: #cfd4dc; }
button.test:hover { background: #f3f4f6; }
button.login { background: #fff; color: #1a1d21; border-color: #cfd4dc; }
button.login:hover { background: #f3f4f6; }
button.clear { background: #fff; color: #b91c1c; border-color: #e3b4b4; }
button.clear:hover { background: #fdecec; }
.result { font-size: .8rem; color: #5a6270; }
.result.ok { color: #15803d; }
.result.err { color: #b91c1c; }
#toast {
  position: fixed; bottom: 1.2rem; left: 50%; transform: translateX(-50%);
  padding: .6rem 1.1rem; border-radius: 8px; color: #fff; font-size: .9rem;
  opacity: 0; pointer-events: none; transition: opacity .2s; z-index: 50;
}
#toast.show { opacity: 1; }
#toast.ok { background: #15803d; }
#toast.err { background: #b91c1c; }
"""


_SCRIPT = """
function showToast(msg, ok) {
  var t = document.getElementById('toast');
  t.textContent = msg;
  t.className = (ok ? 'ok' : 'err') + ' show';
  setTimeout(function () { t.className = t.className.replace(' show', ''); }, 2600);
}

function applyStatus(status) {
  if (!status) return;
  document.querySelectorAll('.card').forEach(function (card) {
    var id = card.getAttribute('data-provider');
    if (!(id in status)) return;
    var badge = card.querySelector('[data-badge]');
    var ok = !!status[id];
    badge.textContent = ok ? 'Configured \\u2713' : 'Not configured';
    badge.className = 'badge ' + (ok ? 'ok' : 'no');
  });
}

async function saveProvider(btn) {
  var card = btn.closest('.card');
  var payload = {};
  card.querySelectorAll('input[data-key]').forEach(function (inp) {
    payload[inp.getAttribute('data-key')] = inp.value;
  });
  try {
    var res = await fetch('/api/save', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload),
    });
    var data = await res.json();
    if (res.ok && data.ok) {
      applyStatus(data.status);
      // Clear inputs so secrets never linger in the DOM.
      card.querySelectorAll('input[data-key]').forEach(function (inp) { inp.value = ''; });
      showToast('Saved', true);
    } else {
      showToast('Save failed: ' + (data.error || res.status), false);
    }
  } catch (e) {
    showToast('Save failed: ' + e, false);
  }
}

async function clearProvider(btn) {
  var card = btn.closest('.card');
  if (!confirm('Remove the stored key(s) for this provider?')) return;
  var keys = [];
  card.querySelectorAll('input[data-key]').forEach(function (inp) {
    keys.push(inp.getAttribute('data-key'));
  });
  try {
    var status = null;
    for (var i = 0; i < keys.length; i++) {
      var res = await fetch('/api/clear', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ field: keys[i] }),
      });
      var data = await res.json();
      if (!(res.ok && data.ok)) {
        showToast('Clear failed: ' + (data.error || res.status), false);
        return;
      }
      status = data.status;
    }
    if (status) applyStatus(status);
    card.querySelectorAll('input[data-key]').forEach(function (inp) { inp.value = ''; });
    showToast('Cleared', true);
  } catch (e) {
    showToast('Clear failed: ' + e, false);
  }
}

async function loginProvider(btn) {
  var card = btn.closest('.card');
  var id = card.getAttribute('data-provider');
  var out = card.querySelector('[data-result]');
  out.textContent = 'A browser window will open; log in, it auto-closes…';
  out.className = 'result';
  btn.disabled = true;
  try {
    var res = await fetch('/api/login/' + encodeURIComponent(id), { method: 'POST' });
    var data = await res.json();
    if (res.ok && data.ok) {
      out.textContent = 'Logged in';
      out.className = 'result ok';
    } else {
      out.textContent = data.error || 'login failed';
      out.className = 'result err';
    }
  } catch (e) {
    out.textContent = String(e);
    out.className = 'result err';
  } finally {
    btn.disabled = false;
  }
}

async function testProvider(btn) {
  var card = btn.closest('.card');
  var id = card.getAttribute('data-provider');
  var out = card.querySelector('[data-result]');
  out.textContent = 'Testing…';
  out.className = 'result';
  try {
    var res = await fetch('/api/test/' + encodeURIComponent(id));
    var data = await res.json();
    if (data.ok) {
      out.textContent = data.count + ' result(s)';
      out.className = 'result ok';
    } else {
      out.textContent = data.error || 'failed';
      out.className = 'result err';
    }
  } catch (e) {
    out.textContent = String(e);
    out.className = 'result err';
  }
}
"""


def _render_page() -> str:
    cards = "".join(_render_provider_card(p) for p in keystore.PROVIDERS)
    cards += _render_network_card()
    note = (
        "Local config tool — bound to 127.0.0.1. Keys are stored at "
        "~/.config/search-mcp/config.json (0600)."
    )
    return f"""<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>search-mcp admin</title>
  <style>{_STYLE}</style>
</head>
<body>
  <div class="wrap">
    <header>
      <h1>search-mcp · provider keys</h1>
      <p class="note">{_esc(note)}</p>
    </header>
    {cards}
  </div>
  <div id="toast"></div>
  <script>{_SCRIPT}</script>
</body>
</html>"""


# --- routes -----------------------------------------------------------------


async def index(request: Request) -> HTMLResponse:
    return HTMLResponse(_render_page())


async def api_status(request: Request) -> JSONResponse:
    return JSONResponse({"providers": keystore.provider_status()})


async def api_save(request: Request) -> JSONResponse:
    try:
        body = await request.json()
    except Exception:
        return JSONResponse({"ok": False, "error": "invalid JSON"}, status_code=400)
    if not isinstance(body, dict):
        return JSONResponse({"ok": False, "error": "expected an object"}, status_code=400)
    # Drop empty-string values: a blank field means "leave unchanged", so it
    # must never reach set_secrets (which would delete the key).
    non_empty = {
        str(k): str(v)
        for k, v in body.items()
        if isinstance(v, (str, int, float)) and str(v).strip() != ""
    }
    if non_empty:
        keystore.set_secrets(non_empty)
    return JSONResponse({"ok": True, "status": keystore.provider_status()})


async def api_clear(request: Request) -> JSONResponse:
    try:
        body = await request.json()
    except Exception:
        return JSONResponse({"ok": False, "error": "invalid JSON"}, status_code=400)
    field = body.get("field") if isinstance(body, dict) else None
    if not field or not isinstance(field, str):
        return JSONResponse({"ok": False, "error": "missing 'field'"}, status_code=400)
    keystore.delete_secret(field)
    return JSONResponse({"ok": True, "status": keystore.provider_status()})


async def api_test(request: Request) -> JSONResponse:
    from .engines import get_engine

    provider_id = request.path_params["provider_id"]
    provider = keystore.provider_by_id(provider_id)
    if provider is None:
        return JSONResponse(
            {"ok": False, "count": 0, "error": f"unknown provider: {provider_id}"},
            status_code=404,
        )
    try:
        engine = get_engine(provider.engine)
        results = await engine.search("openai", 2)
        return JSONResponse({"ok": True, "count": len(results), "error": None})
    except Exception as exc:  # missing key -> ValueError, network -> others
        return JSONResponse({"ok": False, "count": 0, "error": str(exc)})


# Providers that authenticate via an interactive browser login (no API key).
# Maps the provider id to the site the login flow opens.
_LOGIN_URLS: dict[str, str] = {"zhihu": "https://www.zhihu.com"}


async def api_login(request: Request) -> JSONResponse:
    provider_id = request.path_params["provider_id"]
    url = _LOGIN_URLS.get(provider_id)
    if url is None:
        return JSONResponse(
            {"ok": False, "error": f"no browser login for provider: {provider_id}"},
            status_code=404,
        )
    try:
        # Import lazily: the browser pool pulls in Playwright and is only
        # available once the browser part is installed.
        from .browser import pool

        await pool.login(url)
        return JSONResponse({"ok": True, "error": None})
    except Exception as exc:
        return JSONResponse({"ok": False, "error": str(exc)})


app = Starlette(
    routes=[
        Route("/", index, methods=["GET"]),
        Route("/api/status", api_status, methods=["GET"]),
        Route("/api/save", api_save, methods=["POST"]),
        Route("/api/clear", api_clear, methods=["POST"]),
        Route("/api/test/{provider_id}", api_test, methods=["GET"]),
        Route("/api/login/{provider_id}", api_login, methods=["POST"]),
    ]
)


def main() -> None:
    import uvicorn

    # Load SEARCH_MCP_* keys from a local .env so the Test button (and the
    # provider 'configured' badges) reflect .env-supplied keys too.
    keystore.load_env_file_into_environ()
    port = int(os.environ.get("SEARCH_MCP_ADMIN_PORT", "8765"))
    print(f"search-mcp admin → http://127.0.0.1:{port}")
    # Bind to loopback ONLY — this tool reads/writes secrets.
    uvicorn.run(app, host="127.0.0.1", port=port)


if __name__ == "__main__":
    main()
