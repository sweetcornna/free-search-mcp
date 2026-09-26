"""Local settings page: the outbound proxy, and optional provider keys.

A tiny, single-page web app (Starlette + uvicorn, both pulled in by the ``mcp``
dependency — no extra deps, no template engine). The server needs none of it to
work. The proxy card comes first because it is the one setting an install ever
really needs; below it an operator who already has a search-API account can
paste their own key, persisted via :mod:`search_mcp.keystore`.

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

import asyncio
import html
import os
from typing import Any

from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import HTMLResponse, JSONResponse
from starlette.routing import Route

from . import keystore, oauth

# --- HTML rendering (no template engine; build the page as a string) --------


def _esc(text: str) -> str:
    """HTML-escape a value for safe inclusion in markup/attributes."""
    return html.escape(str(text), quote=True)


_PROVIDER_ZH: dict[str, dict[str, object]] = {
    "brave_api": {
        "label": "Brave 搜索 API",
        "free_tier": "每月 2,000 次免费查询",
        "how_to": [
            "打开 Brave Search API 页面并点击 Get started。",
            "注册或登录账号，并完成邮箱验证。",
            "订阅免费的 Data for Search 计划。可能需要绑定卡片，但免费额度不会收费。",
            "进入 dashboard -> API Keys，复制 subscription token。",
        ],
    },
    "serper": {
        "label": "Serper（Google 搜索）",
        "free_tier": "一次性 2,500 次免费查询",
        "how_to": [
            "打开 serper.dev 并注册，Google 登录也可以。",
            "进入 dashboard 后会看到 2,500 次免费额度。",
            "复制 API Key 栏里的密钥。",
        ],
    },
    "tavily": {
        "label": "Tavily（AI 搜索）",
        "free_tier": "每月 1,000 credits 免费",
        "how_to": [
            "打开 app.tavily.com 并注册。",
            "在 dashboard 中找到 API Keys 区域。",
            "复制以 tvly- 开头的密钥。",
        ],
    },
    "google_cse": {
        "label": "Google 自定义搜索",
        "free_tier": "每天 100 次免费查询",
        "how_to": [
            "在 Google Cloud Console 创建 API key。",
            "为项目启用 Custom Search API。",
            "在 Programmable Search Engine 创建搜索引擎，并设置为搜索整个 web。",
            "从控制台复制 Search engine ID，也就是 cx；这里需要同时填写 API key 和 cx。",
        ],
    },
    "anysearch": {
        "label": "AnySearch（可选密钥）",
        "free_tier": "无需密钥也可使用；填写密钥可提高限额",
        "how_to": [
            "AnySearch 可以匿名使用，不填密钥也能工作，只是限额较低。",
            "如需更高限额，注册 anysearch.com 并进入 Console -> API Keys。",
            "创建密钥并粘贴到这里。",
        ],
    },
    "semanticscholar": {
        "label": "Semantic Scholar（可选密钥）",
        "free_tier": "匿名共享额度实测长期 429；申请免费密钥后为每秒 1 次",
        "how_to": [
            "Semantic Scholar 是几个免密钥学术源里元数据最全的：摘要、被引数、开放获取 PDF 直链。",
            "但它的匿名额度是一个共享池，实测基本一直处于限流状态，不填密钥通常拿不到结果。",
            "在 semanticscholar.org/product/api 申请免费密钥，审核通过会发到邮箱（需要几天）。",
            "把密钥粘贴到这里，category=\"paper\" 就会把它一起用上。",
        ],
    },
    "github": {
        "label": "GitHub（可选令牌）",
        "free_tier": "仓库/议题搜索免令牌可用（10 次/分）；令牌提升到 30 次/分并解锁代码搜索",
        "how_to": [
            "github 引擎不带令牌也能搜索仓库和议题。",
            "令牌会提高限额，并启用 github_code，因为 GitHub 的代码搜索接口直接拒绝匿名请求。",
            "在 github.com/settings/tokens 创建，搜索公开仓库不需要任何 scope。",
        ],
    },
    "stackexchange": {
        "label": "Stack Exchange（可选密钥）",
        "free_tier": "匿名每天 300 次；填写密钥后每天 10,000 次",
        "how_to": [
            "不填密钥也能用，只是每天 300 次配额按 IP 共享。",
            "在 stackapps.com/apps/oauth/register 注册应用即可拿到 key。",
            "把 key 粘贴到这里。",
        ],
    },
}

_FIELD_ZH: dict[str, str] = {
    "brave_api_key": "API 密钥",
    "serper_api_key": "API 密钥",
    "tavily_api_key": "API 密钥",
    "google_cse_api_key": "API 密钥",
    "google_cse_cx": "搜索引擎 ID (cx)",
    "anysearch_api_key": "API 密钥（可选）",
    "semanticscholar_api_key": "API 密钥（可选，强烈建议填写）",
    "github_token": "个人访问令牌（可选）",
    "stackexchange_key": "API 密钥（可选）",
    "proxy": "代理 URL",
    "proxy_engines": "仅代理这些引擎（可选，逗号分隔）",
}


def _bilingual(primary: str, zh: str | None) -> str:
    if not zh or zh == primary:
        return _esc(primary)
    return f"{_esc(primary)} <span class=\"zh\">/ {_esc(zh)}</span>"


def _badge_text(configured: bool) -> str:
    return "Configured ✓ / 已配置 ✓" if configured else "Not configured / 未配置"


def _render_provider_card(provider: keystore.Provider) -> str:
    configured = keystore.is_configured(provider.id)
    badge_cls = "ok" if configured else "no"
    badge_txt = _badge_text(configured)

    zh = _PROVIDER_ZH.get(provider.id, {})
    steps = "".join(f"<li>{_esc(step)}</li>" for step in provider.how_to)
    zh_steps = zh.get("how_to", [])
    zh_steps_html = ""
    if isinstance(zh_steps, list) and zh_steps:
        zh_steps_html = (
            '<p class="steps-title">中文</p>'
            + "<ol>"
            + "".join(f"<li>{_esc(str(step))}</li>" for step in zh_steps)
            + "</ol>"
        )

    links = [
        f'<a href="{_esc(provider.signup_url)}" target="_blank" rel="noopener">Sign up / 注册</a>'
    ]
    if provider.docs_url:
        links.append(
            f'<a href="{_esc(provider.docs_url)}" target="_blank" rel="noopener">Docs / 文档</a>'
        )
    links_html = " · ".join(links)

    inputs = []
    for field in provider.fields:
        ftype = "password" if field.secret else "text"
        label_html = _bilingual(field.label, _FIELD_ZH.get(field.key))
        # NEVER pre-fill the value — only ever render an empty input.
        inputs.append(
            f'<label class="field">'
            f'<span class="field-label">{label_html}</span>'
            f'<input type="{ftype}" data-key="{_esc(field.key)}" '
            f'placeholder="{_esc(field.placeholder)}" autocomplete="off" '
            f'spellcheck="false" />'
            f"</label>"
        )
    inputs_html = "".join(inputs)

    # zhihu authenticates via an interactive browser login, not an API key.
    login_btn = ""
    if provider.id == "zhihu":
        login_btn = '<button class="login" onclick="loginProvider(this)">Login / 登录</button>'

    title_html = _bilingual(provider.label, str(zh.get("label", "")))
    free_tier_html = (
        f'<span class="label">Free tier / 免费额度:</span> {_esc(provider.free_tier)}'
    )
    if zh.get("free_tier"):
        free_tier_html += f'<br><span class="zh">{_esc(str(zh["free_tier"]))}</span>'

    return f"""
    <section class="card" data-provider="{_esc(provider.id)}">
      <div class="card-head">
        <h2>{title_html}</h2>
        <span class="badge {badge_cls}" data-badge>{badge_txt}</span>
      </div>
      <p class="free-tier">{free_tier_html}</p>
      <details class="howto">
        <summary>How to get a key / 如何获取密钥</summary>
        <p class="steps-title">English</p>
        <ol>{steps}</ol>
        {zh_steps_html}
        <p class="links">{links_html}</p>
      </details>
      <div class="fields">{inputs_html}</div>
      <div class="actions">
        <button class="save" onclick="saveProvider(this)">Save / 保存</button>
        <button class="test" onclick="testProvider(this)">Test / 测试</button>
        {login_btn}
        <button class="clear" onclick="clearProvider(this)">Clear / 清除</button>
        <span class="result" data-result></span>
      </div>
    </section>
    """


_OAUTH_COPY: dict[str, dict[str, str]] = {
    "codex": {
        "title_zh": "Codex（ChatGPT 账号登录）",
        "about": "OpenAI's own web search, run on your ChatGPT plan (Plus, Pro, Business, …) "
        "through the sign-in the Codex CLI uses. It runs only when a call names "
        'engines=["codex"], and each search counts against your plan\'s Codex usage.',
        "about_zh": "用你的 ChatGPT 账号（Plus、Pro、Business 等套餐）调用 OpenAI 官方的网页搜索，"
        "登录方式与 Codex CLI 相同。只有调用中写明 engines=[\"codex\"] 时才会运行，"
        "每次搜索消耗你自己套餐的 Codex 额度。",
        "note": "Already signed in to the Codex CLI? `search-mcp-login codex --use-codex-cli` "
        "reuses that sign-in read-only instead.",
        "note_zh": "已经登录过 Codex CLI？运行 search-mcp-login codex --use-codex-cli "
        "可以只读复用那份登录。",
    },
}


def _oauth_badge(info: dict[str, Any]) -> tuple[str, str]:
    if not info.get("signed_in"):
        return "no", "Not signed in / 未登录"
    who = info.get("email") or "?"
    return "ok", f"Signed in / 已登录 · {who}"


def _render_oauth_card(spec: oauth.OAuthProvider) -> str:
    info = oauth.status(spec.id)
    badge_cls, badge_txt = _oauth_badge(info)
    copy = _OAUTH_COPY.get(spec.id, {})
    return f"""
    <section class="card oauth" data-oauth="{_esc(spec.id)}">
      <div class="card-head">
        <h2>{_bilingual(spec.label, copy.get("title_zh"))}</h2>
        <span class="badge {badge_cls}" data-badge>{_esc(badge_txt)}</span>
      </div>
      <p class="free-tier">{_esc(copy.get("about", ""))}<br>
        <span class="zh">{_esc(copy.get("about_zh", ""))}</span></p>
      <p class="free-tier">{_esc(copy.get("note", ""))}<br>
        <span class="zh">{_esc(copy.get("note_zh", ""))}</span></p>
      <div class="actions">
        <button class="save" onclick="oauthSignIn(this)">Sign in / 登录</button>
        <button class="test" onclick="oauthTest(this)">Test / 测试</button>
        <button class="clear" onclick="oauthSignOut(this)">Sign out / 退出登录</button>
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
        label_html = _bilingual(field.label, _FIELD_ZH.get(field.key))
        # NEVER pre-fill the value — only ever render an empty input.
        inputs.append(
            f'<label class="field">'
            f'<span class="field-label">{label_html}</span>'
            f'<input type="{ftype}" data-key="{_esc(field.key)}" '
            f'placeholder="{_esc(field.placeholder)}" autocomplete="off" '
            f'spellcheck="false" />'
            f"</label>"
        )
    inputs_html = "".join(inputs)
    configured = keystore.get_secret("proxy") is not None
    badge_cls = "ok" if configured else "no"
    badge_txt = _badge_text(configured)

    return f"""
    <section class="card" data-provider="__network__">
      <div class="card-head">
        <h2>Network / Proxy <span class="zh">/ 网络 / 代理</span></h2>
        <span class="badge {badge_cls}" data-badge>{badge_txt}</span>
      </div>
      <p class="free-tier">
        A proxy fixes datacenter-IP CAPTCHA gating.<br>
        <span class="zh">代理用于解决数据中心 IP 触发 CAPTCHA 或访问限制的问题。</span>
      </p>
      <div class="fields">{inputs_html}</div>
      <div class="actions">
        <button class="save" onclick="saveProvider(this)">Save / 保存</button>
        <button class="clear" onclick="clearProvider(this)">Clear / 清除</button>
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
h2.section { margin: 1.75rem 0 .1rem; font-size: 1.1rem; }
.zh { color: #4b5563; }
.label { font-weight: 600; color: #374151; }
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
.steps-title { margin: .55rem 0 .2rem; font-weight: 600; color: #374151; }
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
function badgeText(ok) {
  return ok ? 'Configured \\u2713 / 已配置 \\u2713' : 'Not configured / 未配置';
}

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
    badge.textContent = badgeText(ok);
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
      showToast('Saved / 已保存', true);
    } else {
      showToast('Save failed / 保存失败: ' + (data.error || res.status), false);
    }
  } catch (e) {
    showToast('Save failed / 保存失败: ' + e, false);
  }
}

async function clearProvider(btn) {
  var card = btn.closest('.card');
  if (!confirm('Remove the stored key(s) for this provider?\\n确认删除该提供商已保存的密钥或配置吗？')) return;
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
        showToast('Clear failed / 清除失败: ' + (data.error || res.status), false);
        return;
      }
      status = data.status;
    }
    if (status) applyStatus(status);
    card.querySelectorAll('input[data-key]').forEach(function (inp) { inp.value = ''; });
    showToast('Cleared / 已清除', true);
  } catch (e) {
    showToast('Clear failed / 清除失败: ' + e, false);
  }
}

async function loginProvider(btn) {
  var card = btn.closest('.card');
  var id = card.getAttribute('data-provider');
  var out = card.querySelector('[data-result]');
  out.textContent = 'A browser window will open; log in, it auto-closes… / 浏览器窗口会打开，请登录，完成后会自动关闭…';
  out.className = 'result';
  btn.disabled = true;
  try {
    var res = await fetch('/api/login/' + encodeURIComponent(id), { method: 'POST' });
    var data = await res.json();
    if (res.ok && data.ok) {
      out.textContent = 'Logged in / 已登录';
      out.className = 'result ok';
    } else {
      out.textContent = data.error || 'login failed / 登录失败';
      out.className = 'result err';
    }
  } catch (e) {
    out.textContent = String(e);
    out.className = 'result err';
  } finally {
    btn.disabled = false;
  }
}

function oauthCard(btn) { return btn.closest('.card'); }

function oauthShow(card, info) {
  if (!info) return;
  var badge = card.querySelector('[data-badge]');
  if (info.signed_in) {
    badge.textContent = 'Signed in / 已登录 · ' + (info.email || '?');
    badge.className = 'badge ok';
  } else {
    badge.textContent = 'Not signed in / 未登录';
    badge.className = 'badge no';
  }
}

async function oauthSignIn(btn) {
  var card = oauthCard(btn);
  var id = card.getAttribute('data-oauth');
  var out = card.querySelector('[data-result]');
  // Opened now, inside the click, so a popup blocker lets it through.
  var tab = window.open('', '_blank');
  out.textContent = 'Starting… / 正在启动…';
  out.className = 'result';
  btn.disabled = true;
  try {
    var res = await fetch('/api/oauth/' + encodeURIComponent(id) + '/start', { method: 'POST' });
    var data = await res.json();
    if (!(res.ok && data.ok)) {
      if (tab) tab.close();
      out.textContent = data.error || 'failed / 失败';
      out.className = 'result err';
      btn.disabled = false;
      return;
    }
    if (tab) {
      tab.location = data.url;
      out.textContent = 'Finish signing in in the new tab… / 请在新标签页完成登录…';
    } else {
      out.innerHTML = '';
      var a = document.createElement('a');
      a.href = data.url; a.target = '_blank'; a.rel = 'noopener';
      a.textContent = 'Open the sign-in page / 打开登录页面';
      out.appendChild(a);
    }
    for (var i = 0; i < 400; i++) {
      await new Promise(function (r) { setTimeout(r, 1500); });
      var p = await (await fetch('/api/oauth/' + encodeURIComponent(id) + '/progress')).json();
      if (p.state === 'done') {
        oauthShow(card, p.status);
        out.textContent = 'Signed in / 登录成功';
        out.className = 'result ok';
        break;
      }
      if (p.state === 'error' || p.state === 'idle') {
        out.textContent = p.error || 'sign-in stopped / 登录已中止';
        out.className = 'result err';
        break;
      }
    }
  } catch (e) {
    out.textContent = String(e);
    out.className = 'result err';
  } finally {
    btn.disabled = false;
  }
}

async function oauthSignOut(btn) {
  var card = oauthCard(btn);
  var id = card.getAttribute('data-oauth');
  if (!confirm('Forget this sign-in on this machine?\\n确认删除本机保存的这份登录吗？')) return;
  var res = await fetch('/api/oauth/' + encodeURIComponent(id) + '/logout', { method: 'POST' });
  var data = await res.json();
  oauthShow(card, data.status);
  showToast('Signed out / 已退出登录', true);
}

async function oauthTest(btn) {
  var card = oauthCard(btn);
  var id = card.getAttribute('data-oauth');
  var out = card.querySelector('[data-result]');
  out.textContent = 'Searching… / 正在搜索…';
  out.className = 'result';
  try {
    var res = await fetch('/api/test/' + encodeURIComponent(id));
    var data = await res.json();
    out.textContent = data.ok ? data.count + ' result(s) / 条结果' : (data.error || 'failed / 测试失败');
    out.className = 'result ' + (data.ok ? 'ok' : 'err');
  } catch (e) {
    out.textContent = String(e);
    out.className = 'result err';
  }
}

async function testProvider(btn) {
  var card = btn.closest('.card');
  var id = card.getAttribute('data-provider');
  var out = card.querySelector('[data-result]');
  out.textContent = 'Testing… / 正在测试…';
  out.className = 'result';
  try {
    var res = await fetch('/api/test/' + encodeURIComponent(id));
    var data = await res.json();
    if (data.ok) {
      out.textContent = data.count + ' result(s) / 条结果';
      out.className = 'result ok';
    } else {
      out.textContent = data.error || 'failed / 测试失败';
      out.className = 'result err';
    }
  } catch (e) {
    out.textContent = String(e);
    out.className = 'result err';
  }
}
"""


def _render_page() -> str:
    # Proxy first, keys second and labelled optional: the order is the message.
    # Led by eight "paste your API key" cards, the page told a no-key project's
    # users that they were missing something.
    network = _render_network_card()
    cards = "".join(_render_provider_card(p) for p in keystore.PROVIDERS)
    oauth_cards = "".join(_render_oauth_card(p) for p in oauth.PROVIDERS.values())
    oauth_note = (
        "One more opt-in engine runs OpenAI's own web search on a ChatGPT account you "
        "sign in with, instead of a key. Sign in here or with `search-mcp-login codex`. "
        "Tokens are stored at ~/.config/search-mcp/oauth/ (0600)."
    )
    oauth_note_zh = (
        "另一个可选引擎不用密钥，而是用你登录的 ChatGPT 账号调用 OpenAI 官方的网页搜索。"
        "可在此处登录，也可以运行 search-mcp-login codex。"
        "令牌保存在 ~/.config/search-mcp/oauth/（0600）。"
    )
    note = (
        "Local config tool, bound to 127.0.0.1. Nothing here is required: search "
        "works with no key. Values are stored at ~/.config/search-mcp/config.json (0600). "
        "本地配置工具，仅绑定 127.0.0.1；无需任何密钥即可搜索，所填内容保存在上述本地文件中。"
    )
    keys_note = (
        "Search already works without any of these. They are for an operator who "
        "has their own account with a provider: brave_api, serper, tavily, google_cse "
        "and github_code run only when a call names them; the rest only raise a "
        "keyless engine's limits."
    )
    keys_note_zh = (
        "不填写任何密钥也能正常搜索。以下内容仅供已有服务商账号的使用者手动填写："
        "brave_api、serper、tavily、google_cse、github_code 只有在调用中被点名时才会运行，"
        "其余密钥只是提高免密钥引擎的额度。"
    )
    return f"""<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>search-mcp · local settings</title>
  <style>{_STYLE}</style>
</head>
<body>
  <div class="wrap">
    <header>
      <h1>search-mcp · local settings <span class="zh">/ 本地设置</span></h1>
      <p class="note">{_esc(note)}</p>
    </header>
    {network}
    <h2 class="section">Optional provider keys <span class="zh">/ 提供商密钥配置（可选）</span></h2>
    <p class="free-tier">{_esc(keys_note)}<br><span class="zh">{_esc(keys_note_zh)}</span></p>
    {cards}
    <h2 class="section">Optional: sign in with an account (OAuth) <span class="zh">/ 账号登录（OAuth，可选）</span></h2>
    <p class="free-tier">{_esc(oauth_note)}<br><span class="zh">{_esc(oauth_note_zh)}</span></p>
    {oauth_cards}
  </div>
  <div id="toast"></div>
  <script>{_SCRIPT}</script>
</body>
</html>"""


# --- routes -----------------------------------------------------------------


def _ui_status() -> dict[str, bool]:
    status = keystore.provider_status()
    status["__network__"] = keystore.get_secret("proxy") is not None
    return status


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
    return JSONResponse({"ok": True, "status": _ui_status()})


async def api_clear(request: Request) -> JSONResponse:
    try:
        body = await request.json()
    except Exception:
        return JSONResponse({"ok": False, "error": "invalid JSON"}, status_code=400)
    field = body.get("field") if isinstance(body, dict) else None
    if not field or not isinstance(field, str):
        return JSONResponse({"ok": False, "error": "missing 'field'"}, status_code=400)
    keystore.delete_secret(field)
    return JSONResponse({"ok": True, "status": _ui_status()})


async def api_test(request: Request) -> JSONResponse:
    from .engines import get_engine

    provider_id = request.path_params["provider_id"]
    provider = keystore.provider_by_id(provider_id)
    if provider is None and provider_id in oauth.PROVIDERS:
        provider = oauth.PROVIDERS[provider_id]
    if provider is None:
        return JSONResponse(
            {"ok": False, "count": 0, "error": f"unknown provider: {provider_id}"},
            status_code=404,
        )
    if provider_id in oauth.PROVIDERS and not oauth.is_signed_in(provider_id):
        # Answered here, not by a search: an unsigned engine may open its own
        # sign-in page when searched (codex over stdio), and this process has
        # a Sign in button for that.
        return JSONResponse({"ok": False, "count": 0, "error": (
            f"{provider_id} not configured: not signed in. Use Sign in / 登录 on this card."
        )})
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


# --- OAuth sign-in ------------------------------------------------------------
# One sign-in per provider at a time. The page starts it, opens the returned
# link in a tab, and polls /progress until the loopback callback (served by
# oauth.login in this same process) has stored the tokens.

_oauth_jobs: dict[str, dict[str, Any]] = {}


def _job_state(provider_id: str) -> dict[str, Any]:
    job = _oauth_jobs.get(provider_id)
    if job is None:
        return {"state": "idle"}
    task: asyncio.Task[Any] = job["task"]
    if not task.done():
        return {"state": "waiting"}
    if task.cancelled():
        return {"state": "error", "error": "the sign-in was cancelled / 登录已取消"}
    exc = task.exception()
    if exc is not None:
        return {"state": "error", "error": str(exc)}
    return {"state": "done"}


async def api_oauth_start(request: Request) -> JSONResponse:
    provider_id = request.path_params["provider_id"]
    if provider_id not in oauth.PROVIDERS:
        return JSONResponse({"ok": False, "error": f"unknown provider: {provider_id}"},
                            status_code=404)
    previous = _oauth_jobs.pop(provider_id, None)
    if previous is not None and not previous["task"].done():
        # Frees the callback port for the new attempt.
        previous["task"].cancel()
        await asyncio.gather(previous["task"], return_exceptions=True)
    link: asyncio.Future[str] = asyncio.get_running_loop().create_future()

    def on_url(url: str) -> None:
        if not link.done():
            link.set_result(url)

    task = asyncio.create_task(
        oauth.login(provider_id, open_browser=False, paste=False, on_url=on_url)
    )
    _oauth_jobs[provider_id] = {"task": task}
    await asyncio.wait({task, link}, return_when=asyncio.FIRST_COMPLETED)
    if link.done():
        return JSONResponse({"ok": True, "url": link.result()})
    exc = task.exception() if not task.cancelled() else None
    return JSONResponse({"ok": False, "error": str(exc) if exc else "sign-in did not start"})


async def api_oauth_progress(request: Request) -> JSONResponse:
    provider_id = request.path_params["provider_id"]
    if provider_id not in oauth.PROVIDERS:
        return JSONResponse({"state": "error", "error": "unknown provider"}, status_code=404)
    return JSONResponse({**_job_state(provider_id), "status": oauth.status(provider_id)})


async def api_oauth_logout(request: Request) -> JSONResponse:
    provider_id = request.path_params["provider_id"]
    if provider_id not in oauth.PROVIDERS:
        return JSONResponse({"ok": False, "error": "unknown provider"}, status_code=404)
    oauth.sign_out(provider_id)
    return JSONResponse({"ok": True, "status": oauth.status(provider_id)})


async def api_oauth_status(request: Request) -> JSONResponse:
    return JSONResponse({pid: oauth.status(pid) for pid in oauth.PROVIDERS})


app = Starlette(
    routes=[
        Route("/", index, methods=["GET"]),
        Route("/api/status", api_status, methods=["GET"]),
        Route("/api/save", api_save, methods=["POST"]),
        Route("/api/clear", api_clear, methods=["POST"]),
        Route("/api/test/{provider_id}", api_test, methods=["GET"]),
        Route("/api/login/{provider_id}", api_login, methods=["POST"]),
        Route("/api/oauth/status", api_oauth_status, methods=["GET"]),
        Route("/api/oauth/{provider_id}/start", api_oauth_start, methods=["POST"]),
        Route("/api/oauth/{provider_id}/progress", api_oauth_progress, methods=["GET"]),
        Route("/api/oauth/{provider_id}/logout", api_oauth_logout, methods=["POST"]),
    ]
)


def _schedule_browser_open(url: str) -> None:
    """Best-effort: open the admin page in the default browser shortly after
    startup (giving uvicorn a moment to bind). Disable with
    SEARCH_MCP_ADMIN_NO_BROWSER=1 (headless boxes, scripts); explicit falsy
    values ("0", "false", "no", "off", "") keep the auto-open."""
    flag = (os.environ.get("SEARCH_MCP_ADMIN_NO_BROWSER") or "").strip().lower()
    if flag and flag not in ("0", "false", "no", "off"):
        return
    import threading
    import webbrowser

    def _open() -> None:
        try:
            webbrowser.open(url)
        except Exception:
            pass

    threading.Timer(1.0, _open).start()


def main() -> None:
    import uvicorn

    # Load SEARCH_MCP_* keys from ./.env and <config_dir>/.env so the Test
    # button (and the provider 'configured' badges) reflect them too.
    keystore.load_all_env_files()
    port = int(os.environ.get("SEARCH_MCP_ADMIN_PORT", "8765"))
    url = f"http://127.0.0.1:{port}"
    print(f"search-mcp admin → {url}")
    _schedule_browser_open(url)
    # Bind to loopback ONLY — this tool reads/writes secrets.
    uvicorn.run(app, host="127.0.0.1", port=port)


if __name__ == "__main__":
    main()
