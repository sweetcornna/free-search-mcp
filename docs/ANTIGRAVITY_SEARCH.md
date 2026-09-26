# Google Search on an Antigravity sign-in: the `antigravity` engine

Read the warning before anything else on this page.

Google's Antigravity terms say: "Using third party software, tools, or services
to access the Service (e.g. using OpenClaw with Antigravity OAuth) is a breach
of this Agreement. Such actions may be grounds for suspension or termination of
your Antigravity and/or Gemini CLI accounts." Google has suspended accounts for
exactly this. This engine is that use. It signs in with the Antigravity IDE's
own OAuth client, read from the Antigravity install on this machine, and the
backend only answers requests that identify as Antigravity, so every search
sends Antigravity's user agent.

It exists because an operator asked for it on their own account. Nothing in
free-search-mcp needs it, no pool, reserve or `category=` route ever reaches it
(`tests/test_no_key_positioning.py` enforces that), and unlike `codex` it never
opens a sign-in page by itself. If you are not sure you accept the risk for the
Google account you would sign in with, do not sign in.

## 中文速览

- 风险：Google 的 Antigravity 条款明确禁止第三方工具使用 Antigravity 的 OAuth 登录，
  后果是封禁 Antigravity 账号，连带 Gemini CLI 账号，而且已经有人因此被封。这个引擎就是
  这种用法：它用 Antigravity IDE 自己的 OAuth 客户端登录，后端只接受自称 Antigravity
  的请求，所以每次搜索都会发送 Antigravity 的 User-Agent。只用你愿意承担这个风险的账号登录。
- 用途：让 Gemini 模型调用 Google 搜索，结果地址全部来自搜索本身（grounding），
  消耗的是你账号的 Antigravity 额度。
- 登录：`uvx --from free-search-mcp search-mcp-login antigravity`，命令会先打印风险警告，
  再打开 Google 授权页，点「允许」即可。也可以在 `search-mcp-admin` 设置页的
  Antigravity 卡片上点「登录」。它不会像 `codex` 那样在第一次调用时自动弹出登录页。
- 客户端：本仓库不包含 Antigravity 的 OAuth 客户端 ID 和密钥。登录时先读
  `SEARCH_MCP_ANTIGRAVITY_CLIENT_ID` 和 `SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET`（环境变量，
  或设置页 Antigravity 卡片里的「Antigravity 的 OAuth 客户端」），都没设就从本机安装的
  Antigravity 程序里读取。装了 Antigravity 的 Mac 什么都不用填；没装的机器要填这两项，
  值可以从已登录机器的 `~/.config/search-mcp/oauth/antigravity.json` 里抄
  （`client_id`、`client_secret` 两个字段）。
- 使用：`search("...", engines=["antigravity"])`。不写 `engines` 时它永远不会参与搜索。
- 模型：默认 `latest`，跟随账号的 Antigravity 模型目录，用 Antigravity 当前提供的 flash
  模型；它没去搜索时，再用 Antigravity 自己做网页搜索的模型问一次。Google 发布新模型后
  自动跟上。写具体模型名则固定使用该模型。
- 在远程机器（SSH）上：浏览器最后跳转到 `http://localhost:51121/oauth-callback?code=...`
  时页面会打不开，复制地址栏里的完整地址，粘贴回终端并回车。
- 网络受限地区：登录和搜索都会走 `SEARCH_MCP_PROXY` 设置的代理。
- 不想用了：`search-mcp-login logout antigravity` 删除本机保存的 token。要彻底撤销授权，
  到 Google 账号的「第三方应用和服务」里移除 Antigravity。

## Sign in

```bash
uvx --from free-search-mcp search-mcp-login antigravity    # plugin / uvx installs
uv run search-mcp-login antigravity                         # a source checkout
```

The command prints the warning above, then opens Google's consent page for
Antigravity. Google shows that page every time, because the sign-in asks for
offline access (a refresh token). Choose the account and click Allow; the tab
then says "Signed in". The settings page (`search-mcp-admin`) has an
Antigravity card with the same warning and a **Sign in / 登录** button.

After the tokens arrive, the sign-in asks the Cloud Code backend which project
and tier the account has (`loadCodeAssist`). `search-mcp-login status` shows
the account, the tier (for example "Google AI Pro") and when the access token
expires. If that lookup fails, the sign-in still succeeds; the searches run
without a project.

The tokens are stored at `~/.config/search-mcp/oauth/antigravity.json`,
owner-only (`0600`). Google access tokens last an hour and are refreshed
automatically; Google does not rotate the refresh token.

```bash
search-mcp-login status                # who is signed in, which tier, token expiry
search-mcp-login logout antigravity    # forget the stored tokens on this machine
```

Signing out here deletes the local tokens only. To revoke the grant itself,
remove Antigravity under "Third-party apps and services" in the Google account.

### Antigravity's OAuth client

This repository does not include Antigravity's OAuth client ID and secret. A
sign-in takes them from `SEARCH_MCP_ANTIGRAVITY_CLIENT_ID` and
`SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET`, set in the environment or under
"Antigravity's OAuth client" on the settings page card. When neither is set,
it reads them from the Antigravity install on this machine: the app's language
server and the `agy` CLI both carry them. Those files hold two Google clients,
and the engine recognises Antigravity's by a SHA-256 fingerprint of each value,
so the values themselves appear nowhere in the code.

On a Mac with Antigravity installed there is nothing to set. The lookup was
checked against `/Applications/Antigravity.app` and `~/.local/bin/agy`; the
Linux and Windows install folders it also tries were not. On a machine without
Antigravity, set both variables. A machine that has signed in keeps them in
`~/.config/search-mcp/oauth/antigravity.json`, as `client_id` and
`client_secret`. If a later Antigravity release changes its client, the lookup
no longer finds it and the sign-in says so; setting the variables works then
too.

The sign-in stores the client it used next to the tokens, because Google
refreshes a token only for the client that issued it. Changing the variables
later does not affect an account that is already signed in.

### Over SSH, or without a browser on this machine

The sign-in returns to `http://localhost:51121/oauth-callback`. That address is
on the client's allow-list, so the port cannot be changed. On a remote machine,
run `search-mcp-login antigravity --no-browser`, open the printed address in
any browser, and sign in. The browser then fails to load the `localhost` page
it is sent to. Copy the whole address from the address bar and paste it into
the terminal.

## Search with it

```python
search("rust 2024 edition changes", engines=["antigravity"])
research("what changed in python 3.14 asyncio", engines=["antigravity"])
```

A search takes about 10 seconds: 4 to 8 for the model and its search, then up to
5 more to turn the result links into page addresses. `freshness=` is passed to
Google Search as
a time range, `include_domains=` and `exclude_domains=` are asked for in the
instruction, and every filter is applied again to the results. Results are
cached like other searches, so a repeated query inside the cache lifetime does
not spend quota again.

Naming it before signing in returns an error that says the search itself is
fine, what to use instead, and that the user should not be asked for a key or a
sign-in. A spent quota is reported with its reset time when Google gives one.

## Settings

| Var | Default | Meaning |
|---|---|---|
| `SEARCH_MCP_ANTIGRAVITY_MODEL` | `latest` | the model that runs the search; `latest` follows the account's catalogue, a model name pins it |
| `SEARCH_MCP_ANTIGRAVITY_TIMEOUT` | `60` | seconds for one search |
| `SEARCH_MCP_ANTIGRAVITY_BASE_URLS` | the daily sandbox, then production | backends tried in order, as a JSON list |
| `SEARCH_MCP_ANTIGRAVITY_VERSION` | `2.1.4` | the Antigravity release the user agent names |
| `SEARCH_MCP_ANTIGRAVITY_CLIENT_ID` | read from the Antigravity install | Antigravity's OAuth client ID, for a machine without Antigravity (see above) |
| `SEARCH_MCP_ANTIGRAVITY_CLIENT_SECRET` | read from the Antigravity install | its client secret; set both or neither |

With `latest`, the engine reads the account's model catalogue
(`fetchAvailableModels`) at most every six hours and follows what Antigravity
itself names there, so a new model is used as soon as Google ships it:

- A search runs on the flash model Antigravity currently offers
  (`tieredModelIds.flash`; `gemini-3.8-flash-tiered` on 2026-09-26).
- When that model answers without searching, the search is asked once more of
  the model Antigravity runs its own web search on (`webSearchModelIds`;
  `gemini-3.1-flash-lite` then).
- When the catalogue cannot be read, `gemini-3.8-flash-tiered` is used and the
  catalogue is asked again five minutes later.
- When the service refuses the model the catalogue named, the search goes to
  the other one, and the next search reads the catalogue again.

The model matters because some do not search. On 2026-09-26, with the engine's
own request over 8 queries, the flash model searched 8 times out of 8 (10 s
median), the web-search model 6 times (4 s), and `gemini-3.5-flash-low` never.
An answer without search results produces no results here (see below), so a
model named in `SEARCH_MCP_ANTIGRAVITY_MODEL` that skips the search looks like
an empty engine.

When the backend starts answering 403 "You do not have a valid license of this
product" to an account that worked before, it has probably stopped accepting
the Antigravity version in the user agent. Set `SEARCH_MCP_ANTIGRAVITY_VERSION`
to the release installed Antigravity reports. The same 403 can also mean
Google has restricted the account.

The sign-in and the searches go through `SEARCH_MCP_PROXY` when it is set.

## How it works

Measured against the backend on 2026-09-26 with Antigravity 2.1.4 installed:

1. `POST {base}/v1internal:generateContent` with the account's project, the
   model, and the `googleSearch` tool. The request carries Antigravity's user
   agent (`antigravity/2.1.4 darwin/arm64` on an Apple Silicon Mac) and its
   client metadata. With any other user agent, every call was refused with 403.
2. The model lists the pages it found, one per line, and the reply's
   `groundingMetadata` says which search results each stretch of text rests on.
   Its offsets count UTF-8 bytes, which matters for any query outside ASCII.
3. Every result URL comes from those search results. Each is a
   `vertexaisearch.cloud.google.com` redirect, resolved to the page it points
   to. The lookups run at once, at most twice as many as the results asked
   for, and one that takes longer than 5 seconds is dropped, as is one that
   does not resolve. A URL the model wrote in its own text is never used; in
   testing it named hosts that did not exist.
4. A reply with no search results means the model answered from memory. The
   engine asks once more, and a second such reply yields no results.

The production host (`cloudcode-pa.googleapis.com`) answered every search with
429 for a free-tier account that the daily sandbox host served, so the sandbox
is tried first and production second. Either host out of quota or failing hands
over to the next one.

This is an undocumented backend that Google can change at any time. When it
changes, the engine returns an error and the keyless engines are unaffected.
