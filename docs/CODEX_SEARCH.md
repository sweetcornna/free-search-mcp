# Searching on your ChatGPT plan: the `codex` engine

You can skip this page. Nothing in free-search-mcp needs an account, and no
pool, reserve or `category=` route ever reaches this engine on its own
(`tests/test_no_key_positioning.py` enforces that).

It is for an operator who already has a ChatGPT plan that includes Codex and
wants the server to use OpenAI's own web search, the one Codex itself uses.
There is no API key and no API bill: every search counts against the plan's
Codex usage, the same way a search inside Codex does. OpenAI supports signing
in with ChatGPT from third-party tools.

## 中文速览

- 用途：用你自己的 ChatGPT 账号（含 Codex 的套餐，如 Plus、Pro、Business）调用 OpenAI
  官方网页搜索，也就是 Codex 自己用的那个搜索。不需要 API key，也不产生 API 费用，
  消耗的是套餐里的 Codex 额度。
- 登录：运行 `uvx --from free-search-mcp search-mcp-login codex`，浏览器会打开和
  `codex login` 相同的授权页；或者在 `search-mcp-admin` 设置页点击「登录」。
- 已经登录过 Codex CLI：运行 `search-mcp-login codex --use-codex-cli`，只读复用
  `~/.codex/auth.json`，本服务不会改写那份文件。
- 在远程机器（SSH）上：浏览器最后跳转到 `http://127.0.0.1:1455/auth/callback?code=...`
  时页面会打不开，这是正常的。复制地址栏里的完整地址，粘贴回终端并回车即可。
- 使用：`search("...", engines=["codex"])`。不写 `engines` 时它永远不会自动参与搜索。
- 网络受限地区：登录和搜索都会走 `SEARCH_MCP_PROXY` 设置的代理。
- 为什么没有 Antigravity：Antigravity 的服务条款明确禁止第三方工具使用它的 OAuth
  登录，Google 会因此封禁账号（连带 Gemini CLI）。Gemini API 的 Google 搜索
  grounding 条款也禁止把结果链接抽取出来缓存或建索引。所以这里不提供 Google 的对应功能。

## Sign in

```bash
uvx --from free-search-mcp search-mcp-login codex    # plugin / uvx installs
uv run search-mcp-login codex                         # a source checkout
```

A browser opens the same consent page `codex login` shows. Approve it and the
page says "Signed in". The tokens are stored at
`~/.config/search-mcp/oauth/codex.json`, owner-only (`0600`), and refreshed
automatically before they expire. OpenAI rotates the refresh token on every
refresh, so the refresh runs under a lock: two servers sharing the file never
spend the same refresh token twice.

The settings page does the same from a button. Run `search-mcp-admin`, open
http://127.0.0.1:8765, and click **Sign in / 登录** on the Codex card. The card
also has **Test / 测试** and **Sign out / 退出登录**.

```bash
search-mcp-login status          # who is signed in, which plan, token expiry
search-mcp-login logout codex    # forget the stored tokens on this machine
```

### Over SSH, or without a browser on this machine

The sign-in returns to `http://127.0.0.1:1455/auth/callback` (or port 1457 when
1455 is taken). Those are the only addresses the Codex client allows, so the
port cannot be changed. On a remote machine, run
`search-mcp-login codex --no-browser`, open the printed address in any
browser, and sign in. The browser then fails to load the `127.0.0.1` page it
is sent to. That is expected: copy the whole address from the address bar and
paste it into the terminal.

### Reusing the Codex CLI's sign-in

```bash
search-mcp-login codex --use-codex-cli             # $CODEX_HOME/auth.json, else ~/.codex/auth.json
search-mcp-login codex --use-codex-cli /path/to/auth.json
```

This stores a link, not a copy of the tokens, and the link is read-only. This
server never refreshes or rewrites Codex's file, because a refresh here would
rotate the CLI's refresh token and sign the CLI out. When the linked access
token expires, the engine says so; running `codex` once refreshes it. For a
sign-in that keeps itself fresh, use `search-mcp-login codex` instead. It has
its own tokens and never touches the CLI's.

A Codex login that uses an API key, or keeps its credentials in the OS keyring
rather than in `auth.json`, has nothing to link.

## Search with it

```python
search("rust 2024 edition changes", engines=["codex"])
research("what changed in python 3.14 asyncio", engines=["codex"])
```

Filters work as usual. `freshness=` and `include_domains=` are passed to
OpenAI's search, and every filter is applied again to the results, as for any
other engine. Results are cached like other searches, so a repeated query
inside the cache lifetime does not spend usage again.

Naming it before signing in returns an error that says the search itself is
fine, what to use instead, and that the user should not be asked for a key or a
sign-in. When the plan's usage window is used up, the error says so and when it
resets. Neither counts as an engine failure in the circuit breaker.

## Settings

| Var | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_CODEX_MODEL` | `gpt-6-luna` | the model the search runs under; any model the plan offers |
| `SEARCH_MCP_CODEX_REASONING_EFFORT` | `low` | only used by the fallback below |
| `SEARCH_MCP_CODEX_TIMEOUT` | `60` | seconds for one search |
| `SEARCH_MCP_CODEX_BASE_URL` | `https://chatgpt.com/backend-api/codex` | the Codex backend |

The sign-in and the searches go through `SEARCH_MCP_PROXY` when it is set, which
matters where `auth.openai.com` or `chatgpt.com` is blocked.

## How it works

The engine calls the Codex backend the way the current Codex CLI does
(`openai/codex`, read on 2026-09-26):

1. `POST /alpha/search` with a search command. This is the search endpoint
   current Codex models use, and it answers with structured results: each
   result's title, URL and snippet come straight from OpenAI's search.
2. When a deployment has no such endpoint, `POST /responses` with the hosted
   `web_search` tool. A model runs the search and lists what it found, and
   every result URL comes from a citation the search attached to its line. A
   URL that appears only in the model's own words is dropped.

This is an undocumented backend that OpenAI can change at any time. When it
changes, the engine returns an error and the keyless engines are unaffected.

## Why there is no Antigravity engine

Google's Antigravity terms say: "Using third party software, tools, or services
to access the Service (e.g. using OpenClaw with Antigravity OAuth) is a breach
of this Agreement. Such actions may be grounds for suspension or termination of
your Antigravity and/or Gemini CLI accounts." Google has suspended accounts for
exactly this. The supported alternative, Grounding with Google Search on the
Gemini API, forbids caching grounded results and using automated means to
collect their links, which is what a search engine in this server would have
to do. So this server does not offer either.
