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
- 登录：最简单的方式是直接用。第一次调用 `engines=["codex"]` 而还没登录时，服务器会
  自动在本机浏览器里打开 ChatGPT 授权页，你点同意后这次搜索会接着完成（最多等 45 秒；
  更久的话授权页仍保留 10 分钟，批准后重新搜索即可）。也可以手动运行
  `uvx --from free-search-mcp search-mcp-login codex`（同样会自动打开浏览器），
  或者在 `search-mcp-admin` 设置页点击「登录」。不想自动弹出时设置
  `SEARCH_MCP_CODEX_AUTO_SIGNIN=false`。
- 已经登录过 Codex CLI：运行 `search-mcp-login codex --use-codex-cli`，只读复用
  `~/.codex/auth.json`，本服务不会改写那份文件。
- 在远程机器（SSH）上：浏览器最后跳转到 `http://127.0.0.1:1455/auth/callback?code=...`
  时页面会打不开，这是正常的。复制地址栏里的完整地址，粘贴回终端并回车即可。
- 使用：`search("...", engines=["codex"])`。不写 `engines` 时它永远不会自动参与搜索。
- 模型：默认 `latest`，跟随套餐的模型目录，用最新一代里最轻的、能搜索且未退役的模型
  （2026-09-26 为 gpt-6-luna；同一代三个模型的搜索结果相同，最轻的最快、最省额度）。
  新一代模型上线后自动跟上。写具体模型名则固定使用该模型。
- 网络受限地区：登录和搜索都会走 `SEARCH_MCP_PROXY` 设置的代理。
- Google 的对应功能：另有一个 `antigravity` 引擎，用 Antigravity IDE 的登录调用
  Google 搜索。它违反 Google 的 Antigravity 条款，有封号风险（连带 Gemini CLI），
  使用前务必先读 [ANTIGRAVITY_SEARCH.md](ANTIGRAVITY_SEARCH.md)。

## Sign in

The easiest way is to use it. The first time a call names `engines=["codex"]`
with no sign-in stored, the server opens the ChatGPT sign-in page in this
machine's browser, and the search carries on as soon as you approve it. One
search waits up to 45 seconds (`SEARCH_MCP_CODEX_SIGNIN_WAIT_SECONDS`). If you
take longer, it returns a message saying the page is open, the page stays
answerable for ten minutes, and the next search picks the sign-in up.

This only happens where someone can see the page: with the default stdio
transport (the server runs on your desktop) and where a desktop browser can be
started (macOS, Windows, or Linux with a display and `xdg-open`). At most one
unanswered page is opened per server run; after one is closed or refused, the
error points at `search-mcp-login codex` instead of opening another. The
browser is started with its output detached, because over stdio the server's
stdout is the MCP connection. Set `SEARCH_MCP_CODEX_AUTO_SIGNIN=false` to turn
it off.

To sign in ahead of time:

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
| `SEARCH_MCP_CODEX_MODEL` | `latest` | the model the search runs under; `latest` follows the plan's catalogue (below), any model name the plan offers pins it |
| `SEARCH_MCP_CODEX_REASONING_EFFORT` | `low` | only used by the fallback below |
| `SEARCH_MCP_CODEX_TIMEOUT` | `60` | seconds for one search |
| `SEARCH_MCP_CODEX_BASE_URL` | `https://chatgpt.com/backend-api/codex` | the Codex backend |
| `SEARCH_MCP_CODEX_AUTO_SIGNIN` | `true` | open the sign-in page by itself on first use (stdio, desktop only) |
| `SEARCH_MCP_CODEX_SIGNIN_WAIT_SECONDS` | `45` | how long one search waits for that approval |

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

With `SEARCH_MCP_CODEX_MODEL=latest`, the engine reads the plan's model
catalogue (`GET /models`) at most every six hours and uses the newest
generation's lightest model that can search and is not being retired
(`gpt-6-luna` on 2026-09-26). The three GPT-6 models returned the same results
for the same searches, so the lightest one, the fastest and the one that spends
least of the plan, is the one used; a new generation is used as soon as the
catalogue lists it. The backend lists a model only to clients new enough for
it, so the catalogue is asked as the newer of the Codex CLI installed on the
machine and 0.155.0. When the catalogue cannot be read, `gpt-6-luna` is used.

This is an undocumented backend that OpenAI can change at any time. When it
changes, the engine returns an error and the keyless engines are unaffected.

## Google's counterpart

There is an `antigravity` engine that runs Google Search on the sign-in of
Google's Antigravity IDE. Google's terms forbid that use of the sign-in and
Google has suspended accounts for it, so it never opens a sign-in by itself.
Read [ANTIGRAVITY_SEARCH.md](ANTIGRAVITY_SEARCH.md) before using it.
