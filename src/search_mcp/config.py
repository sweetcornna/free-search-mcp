from pathlib import Path
from typing import Literal

from pydantic import Field, SecretStr, field_validator
from pydantic_settings import BaseSettings, SettingsConfigDict

from .keystore import load_all_env_files

DEFAULT_CACHE_DIR = Path.home() / ".cache" / "search-mcp"

# ONE loader defines where config lives and its precedence (real env >
# ./.env > <config_dir>/.env) for BOTH pydantic settings and the keystore:
# populate os.environ before Settings reads it, instead of a parallel
# pydantic env_file list that would have to mirror the same paths forever.
# It loads ./.env first, so a SEARCH_MCP_CONFIG_DIR set there is honored
# when the config-dir .env is resolved. (keystore is stdlib-only — no cycle.)
load_all_env_files()


class Settings(BaseSettings):
    model_config = SettingsConfigDict(env_prefix="SEARCH_MCP_", extra="ignore")

    cache_dir: Path = DEFAULT_CACHE_DIR
    cache_ttl_seconds: int = 60 * 60 * 24 * 7
    # Size cap on the SQLite cache file (db + WAL). Enforced opportunistically
    # (connection init + every N writes) by dropping the oldest pages rows and
    # vacuuming; 0 disables the cap. Expired rows are purged on the same cadence.
    cache_max_mb: int = 512

    # The default pool: keyless, plain-HTTP, general web indexes. Measured
    # again 2026-10-29. The engines that only served a CAPTCHA, a JavaScript
    # wall or nothing at all — mojeek, google, serpsearch, startpage, searx and
    # sogou — were removed from the registry instead of being left for the
    # health tracker to bench on every fresh process.
    #   * duckduckgo  — reliable; curl_cffi's browser fingerprint avoids the
    #                   anomaly 202s. Coherent on every query measured.
    #   * bing        — real results again with a warmed session and the right
    #                   request shape (engines/bing.py). When it serves a decoy
    #                   page instead, the off-topic guard drops it.
    #   * yahoojp     — Yahoo! JAPAN serves Google's index as plain HTML, in
    #                   any language. Measured 2026-10-31: 0.8 to 1.5 s, and
    #                   docs.python.org / Stack Overflow / the Fed's own page
    #                   on top where the others had blogs. Not served in the
    #                   EEA or the UK: there it is a `region` gate, benched,
    #                   and the other three carry on.
    #   * anysearch   — keyless anonymous tier of a JSON API, ~3s, coherent.
    # `googlenews` left this list. It answered every query, programming
    # questions included, with ten news headlines behind opaque redirect URLs;
    # see `fresh_engines`.
    default_engines: list[str] = ["duckduckgo", "bing", "yahoojp", "anysearch"]
    # Stand-ins, in order, used only while fewer than `min_healthy_engines`
    # general engines are healthy. yahoo is Bing's index through Yahoo's own
    # front end (on topic where bing served its decoy), seznam an independent
    # crawler, so360 plain HTTP and coherent; brave works but needs a browser
    # render (seconds, and Chromium). Not used to pad a pool that is merely
    # one short — three good engines beat three good engines plus a browser
    # render.
    reserve_engines: list[str] = ["yahoo", "seznam", "so360", "brave"]
    min_healthy_engines: int = 3
    # Joined to the pool when the caller asks for `freshness="day"|"week"` —
    # the only time a news feed is the right answer to a web search.
    fresh_engines: list[str] = ["googlenews"]
    # Joined when the QUERY is written in that language (detected from its
    # script, not from SEARCH_MCP_REGION). Measured: for a Chinese query so360
    # returns the organiser's own site and the university notice that the
    # western indexes do not have at all; quark (Alibaba's Shenma index) adds
    # CSDN, Tencent Cloud and php.cn pages neither has. naver is Korea's own
    # index. Japanese needs no entry: yahoojp is in the default pool.
    locale_engines: dict[str, list[str]] = {"zh": ["so360", "quark"], "ko": ["naver"]}

    # Circuit breaker (health.py). An engine that hits a wall, or keeps
    # failing, is benched for `engine_cooldown_seconds`, doubling on each
    # repeat up to the maximum; one success clears it.
    health_enabled: bool = True
    engine_cooldown_seconds: float = 600.0
    engine_cooldown_max_seconds: float = 3600.0

    max_results_per_engine: int = 10

    # Aggregation-level keyless rescue. When a FRESH search comes back empty —
    # or nearly empty while demonstrably unhealthy (engine errors, gates, or a
    # silent zero) — the aggregator makes one bounded recovery attempt via
    # these engines, in order, first hit wins. A healthy sparse result never
    # triggers it, so the normal path pays nothing.
    # When a search asks for a `category` but not for specific engines, the
    # aggregator adds engines that natively index that category (arxiv for
    # "paper", googlenews for "news", ...). Each one is another round trip, so
    # cap how many get pulled in; registry order decides which. 0 disables the
    # routing entirely and restores pure default-pool behavior.
    category_engine_limit: int = 3

    # Route a question to the record sources that claim it (Engine.claims)
    # when the caller passed no `category`: "100 usd to cny" reaches the ECB
    # rate, "CVE-2024-3094" the NVD record, "上海明天天气" the forecast, without
    # the agent knowing the category tree. At most `claim_engine_limit` such
    # engines join the pool, in registry order. They answer in well under a
    # second and run in parallel with the web engines, so the search is no
    # slower for them.
    auto_route_enabled: bool = True
    claim_engine_limit: int = 3

    # How long one search waits for its slowest engine once at least one
    # engine has answered with results. The fan-out returns when every engine
    # is done or this many seconds have passed, whichever is first; engines
    # still running are cancelled and reported as timed out. Measured
    # 2026-09-21: the pool answers in 2 to 4 s, and the tail was one
    # browser-rendered engine taking 15 s or more. 0 disables the deadline.
    search_deadline_seconds: float = 10.0

    # Drop a web engine's whole bucket when its results mention nothing of the
    # query beyond the first word while another engine's do — Bing's decoy
    # page, or a weak rescue source (see coherence.py). Off restores the old
    # behaviour of merging whatever came back.
    coherence_guard_enabled: bool = True

    rescue_enabled: bool = True
    # Tried after `reserve_engines`, skipping whatever already ran or is benched.
    # youcom is keyless but quota-limited, which fits a rare recovery pass;
    # baidu is the second independent index and needs no key.
    rescue_engines: list[str] = ["youcom", "baidu"]
    # Wall clock for the whole rescue pass.
    rescue_timeout: float = 10.0
    # The pass is a staggered race (hedged requests): the first candidate is
    # asked at once, the next one this many seconds later or as soon as the one
    # before it fails, and the first usable answer wins and cancels the rest.
    # It used to try one candidate at a time, each in an equal slice of
    # `rescue_timeout`, so a slow first candidate cost its whole slice before
    # the next was asked. 0 asks every candidate at once.
    rescue_stagger_seconds: float = Field(default=1.0, ge=0)

    rate_limit_per_minute: int = Field(default=30, gt=0)
    fetch_rate_limit_per_minute: int = Field(default=20, gt=0)

    request_timeout: float = 15.0
    fetch_timeout: float = 25.0

    user_agent: str = (
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) "
        "AppleWebKit/537.36 (KHTML, like Gecko) "
        "Chrome/131.0.0.0 Safari/537.36"
    )
    accept_language: str = "en-US,en;q=0.9"

    fetch_strategy: Literal["auto", "http", "browser"] = "auto"
    browser_headless: bool = True
    browser_pool_size: int = 2
    # Ceiling, in seconds, on starting the Playwright driver and on launching
    # the browser. Without it a launch that hangs (a proxy, VPN or TUN dropping
    # the driver handshake) holds BrowserPool's lock forever, and every later
    # fetch waits behind it. A healthy launch takes 1 to 3 seconds.
    browser_launch_timeout: float = Field(default=30.0, gt=0)
    # After a launch times out, browser calls fail at once for this many
    # seconds, so a sustained outage does not cost every concurrent fetch the
    # full timeout. A missing binary is not affected: that lasts until restart.
    browser_launch_cooldown: float = Field(default=20.0, ge=0)
    # When the server starts, load the search and fetch code, mint Bing's
    # cookie jar and, if the default pool has needed a browser lately, launch
    # it, in the background. Measured 2026-10-22: the first search otherwise
    # waited 1.6 s for the jar, 2 to 3 s for the browser start, and up to 5 s
    # for the imports in a freshly installed environment.
    prewarm: bool = True
    max_content_chars: int = 50_000

    safesearch: Literal["strict", "moderate", "off"] = "moderate"
    region: str = "us-en"

    # Scholarly APIs (OpenAlex, Crossref, NCBI E-utilities) ask callers to
    # identify themselves and route them to a faster, more reliable pool when
    # they do. Optional — every one of them also serves anonymous traffic.
    contact_email: str = ""

    log_level: str = "INFO"

    # --- MCP transport ----------------------------------------------------
    # stdio stays the default: it needs no port, no origin checks, and it is
    # what every `uvx search-mcp` entry in a client config already expects.
    # streamable-http is for running the server as a shared service — protocol
    # revision 2026-07-28 removed sessions entirely, so HTTP deployments no
    # longer need sticky routing.
    transport: Literal["stdio", "streamable-http"] = "stdio"
    # Loopback by default. Binding 0.0.0.0 exposes an UNAUTHENTICATED server
    # that will fetch arbitrary URLs on the caller's behalf — put it behind a
    # reverse proxy that terminates auth before doing that.
    http_host: str = "127.0.0.1"
    http_port: int = 8000
    http_path: str = "/mcp"
    # Extra Origin values accepted by the DNS-rebinding guard (comma/space
    # separated). Loopback origins are always allowed; add entries here when a
    # browser-based client is served from another origin.
    http_allowed_origins: str = ""

    # --- Tool surface ------------------------------------------------------
    # Comma or space separated allow-list of tool names; empty registers every
    # tool. An embedder that only needs `search` and `fetch` pays for two tool
    # schemas in its model's context instead of eleven, and the answer agent's
    # own child server (agent.py) runs with three.
    tools: str = ""

    # Tool calls this process will run before it starts answering "budget used
    # up, answer now" instead; 0 = no cap. It is process-wide, so it only suits
    # a server started for one job. The answer agent sets it on the child
    # server it hands to a CLI, because a CLI gives no way to forbid tool use
    # on the last turn and a small model does not count its own calls.
    tool_call_budget: int = Field(default=0, ge=0)

    # --- Answer agent (agent.py) -------------------------------------------
    # Off by default, and the `ask` tool is not registered while it is off, so
    # a default install carries no extra tool and needs no model anywhere.
    # Search stays keyless in every mode: this block only says which language
    # model, if any, turns the pages that search found into a short answer.
    #   "api"         - an OpenAI-compatible or Anthropic-compatible HTTP
    #                   endpoint the operator names. A local one (Ollama, LM
    #                   Studio, vLLM) needs no key.
    #   "claude-code" - one headless `claude -p` run, on the operator's login.
    #   "codex"       - one `codex exec` run in a read-only sandbox.
    # A host that has its own subagents (Claude Code, Codex) needs none of
    # this: it runs the shipped agent definition against the ordinary tools.
    agent_backend: Literal["off", "api", "claude-code", "codex"] = "off"
    # Required for "api". For the CLI backends an empty value means "haiku" on
    # Claude Code and the CLI's own default on Codex.
    agent_model: str = ""
    agent_api_protocol: Literal["openai", "anthropic"] = "openai"
    # "openai": the URL that ends in /v1 (https://api.openai.com/v1,
    # http://localhost:11434/v1). "anthropic": the host, with or without /v1.
    agent_api_base_url: str = ""
    agent_api_key: SecretStr = SecretStr("")
    # Path or name of the CLI binary when it is not `claude` / `codex` on PATH.
    # GUI hosts start MCP servers with a bare PATH, so this is often needed.
    agent_command: str = ""
    # Extra CLI flags, shell-split, REPLACING the backend's default. For codex
    # the default is `--ignore-user-config -c model_reasoning_effort="low"`:
    # measured 15 s against 18.5 s, and it keeps the operator's other plugins
    # and MCP servers out of the child. An operator whose provider lives in
    # config.toml should keep that flag and add `-c model_provider=...`
    # overrides here. An empty string means no extra flags at all.
    agent_cli_args: str | None = None
    # Pages the server reads BEFORE the model is called. The model usually
    # answers from these in one call, which is what makes the agent fast.
    agent_depth: int = Field(default=3, ge=1, le=8)
    # How long those reads may take together. A page still loading after this
    # is left out and its search snippet is used, so one slow site cannot hold
    # the answer for the whole fetch timeout.
    agent_read_seconds: float = Field(default=8.0, gt=0)
    # Tool rounds the model may spend after that. 0 = answer from the seed only.
    # Each round measured about 10 s on the CLI backends, so the default is one.
    agent_max_steps: int = Field(default=1, ge=0, le=8)
    # Characters of each page handed to the model.
    agent_max_source_chars: int = Field(default=6000, ge=500)
    # Wall clock for the model part. Past it the caller gets the pages instead.
    agent_timeout_seconds: float = Field(default=90.0, gt=0)
    # Run the answer agent on every `search` too, beside the engine fan-out,
    # and put its short sourced answer above the result list. This is what a
    # configured agent is for: the caller gets both the links and the reading
    # without spending a second turn. Set false to keep `ask` opt-in, or name
    # engines in a call when you want a raw index result.
    agent_in_search: bool = True
    # `search` waits this long for the agent leg; past it the search returns
    # its links and a note saying the answer timed out. The agent's own
    # `agent_timeout_seconds` still caps the model turn inside this window.
    agent_search_timeout_seconds: float = Field(default=45.0, gt=0)

    # --- The agent lane (aggregator.py) -------------------------------------
    # Agent engines search with a language model on the operator's own sign-in
    # (`codex`: OpenAI's web search; `antigravity`: Google Search via Gemini).
    # Those named here join every automatic search (no `engines=`, no
    # image/dataset category) as a lane of their own: they are asked at the
    # same moment as the keyless engines, and what they find is fused into the
    # same ranking. The keyless pool is never held back for them past
    # `agent_lane_wait_seconds`.
    #   "auto" - every signed-in agent engine whose provider allows this use,
    #            which today is `codex`. `antigravity` is never seated by
    #            `auto`, because Google's terms forbid this use of its sign-in;
    #            name it to accept that.
    #   "off"  - none; an agent engine runs only when a call names it.
    #   names  - comma or space separated, e.g. "codex,antigravity".
    # An agent engine with no stored sign-in is never seated and never opens a
    # sign-in page, so nothing changes for an operator who never signed in.
    agent_engines: str = "auto"
    # How long a search waits for the agent lane, counted from the moment the
    # engines were asked. Measured 2026-09-26: `codex` answers in about 3 s and
    # the keyless pool in 2 to 4 s, so the two usually land together; an
    # `antigravity` search takes 10 to 20 s. A lane engine still searching at
    # this point is not cancelled: it finishes in the background and its
    # results are merged into the cached result set, which the next identical
    # search (and `cache://search/...`) returns. When the keyless engines found
    # nothing, the wait stretches to `search_deadline_seconds`.
    agent_lane_wait_seconds: float = Field(default=6.0, ge=0)

    # --- The `codex` sign-in engine (oauth.py, engines/codex.py) ----------
    # Never in a pool or a route. It runs when a call names it, and, once the
    # operator signed in with `search-mcp-login codex`, in the agent lane above.
    # A search can take several seconds (and a model turn, on a deployment
    # without the search endpoint), so it gets a timeout of its own.
    codex_timeout: float = Field(default=60.0, gt=0)
    # `latest` follows the plan's model catalogue: the newest generation's
    # fast, affordable model, looked up at most every six hours, so a new
    # generation is used as soon as the backend lists it (engines/codex.py,
    # pick_model). The search is OpenAI's and returned the same results on
    # every GPT-6 model. Any model name the plan offers pins that model.
    codex_model: str = "latest"
    codex_reasoning_effort: Literal["low", "medium", "high"] = "low"
    codex_base_url: str = "https://chatgpt.com/backend-api/codex"
    # Named with no sign-in stored, open the ChatGPT sign-in page in this
    # machine's browser and carry on with the search once it is approved,
    # instead of failing. Only over stdio (the server runs on the operator's
    # desktop) and only where a desktop browser can be started. At most one
    # unanswered page per server process: after one is closed or refused, the
    # error points at `search-mcp-login codex` instead.
    codex_auto_signin: bool = True
    # How long one search waits for that approval. The page stays answerable
    # for ten minutes, and a search that stops waiting leaves it open for the
    # next. Kept under the 60 s tool timeout some hosts apply.
    codex_signin_wait_seconds: float = Field(default=45.0, ge=0)

    # --- The `antigravity` sign-in engine (oauth.py, engines/antigravity.py) --
    # Google Search run by a Gemini model on the operator's own Antigravity
    # sign-in (`search-mcp-login antigravity`). Opt-in like `codex`, and never
    # opened by itself: Google's terms forbid this use of the sign-in and
    # suspend accounts for it, so an operator signs in on purpose or not at all.
    antigravity_timeout: float = Field(default=60.0, gt=0)
    # `latest` follows the account's Antigravity catalogue: the flash model it
    # currently offers, then the model Antigravity runs its own web search on
    # when the first answers without searching (engines/antigravity.py,
    # pick_models). Looked up at most every six hours. A model name pins that
    # model; some answer from memory instead of searching (on 2026-09-26
    # gemini-3.5-flash-low never searched), and yield no results here.
    antigravity_model: str = "latest"
    # Tried in order: the next one is used when one is out of quota (429),
    # failing (5xx) or unreachable. On 2026-09-26 the production host refused
    # every search with 429 for a free-tier account the sandbox served.
    antigravity_base_urls: list[str] = [
        "https://daily-cloudcode-pa.sandbox.googleapis.com",
        "https://cloudcode-pa.googleapis.com",
    ]
    # The backend licenses only requests that identify as the Antigravity
    # client, by user agent. This is the release the user agent names; raise
    # it when Antigravity updates and the backend starts refusing older ones.
    antigravity_version: str = "2.1.4"

    # --- Safety / sandbox knobs -------------------------------------------
    # SSRF guard escape hatch: when False (default) URLs that resolve to
    # loopback/link-local/private/reserved addresses are rejected.
    allow_private_hosts: bool = False
    # When the SSRF guard runs its resolve-every-A/AAAA layer.
    #   "auto" (default) — only when local DNS actually decides what gets
    #       connected to. It is skipped behind an outbound proxy (the proxy
    #       resolves and connects) and on a fake-IP VPN (every hostname is
    #       mapped into a tunnel range, so the answer is a handle, not a
    #       destination). Both cases previously refused every fetch, which is
    #       what pushes operators to set allow_private_hosts=true and give up
    #       the whole guard.
    #   "always" — run it regardless. Correct only where local DNS agrees with
    #       the egress path.
    #   "never" — skip it entirely.
    # The DNS-free layers (scheme, address literals, internal hostnames) run in
    # every mode, so none of these values opens loopback or a metadata endpoint.
    ssrf_resolve_addresses: Literal["auto", "always", "never"] = "auto"
    # read_doc local-file sandbox root. None (default) DISABLES local file
    # reads entirely — the user opts in by pointing this at a directory.
    document_root: Path | None = None
    # Download sandbox. Enabled by default under cache_dir/downloads; operators
    # can disable it explicitly or override the destination directory.
    download_enabled: bool = True
    download_dir: Path | None = None
    # Downloads are ephemeral. Files older than this are deleted before the
    # next download and at startup; 0 disables the purge (files kept forever).
    download_ttl_hours: int = Field(default=24, ge=0)
    download_max_mb: int = Field(default=100, ge=0)
    # Response-bomb guard: cap on remote response body bytes.
    max_response_bytes: int = 25_000_000
    # Decompression-bomb guard: max PDF pages to parse.
    max_pdf_pages: int = 200
    # Cap on extracted document text (distinct from max_content_chars, which
    # is the fetch-truncation knob for web pages).
    max_document_chars: int = 2_000_000

    # SEARCH_MCP_DOCUMENT_ROOT="" must mean "no local reads". Unvalidated it
    # parsed as Path("."), which opened the launch directory instead.
    @field_validator("download_dir", "document_root", mode="before")
    @classmethod
    def empty_download_dir_uses_default(cls, value: object) -> object:
        if isinstance(value, str):
            value = value.strip()
            if not value:
                return None
        return value

    def enabled_tools(self) -> frozenset[str]:
        """The `tools` allow-list as a set; empty means every tool."""
        return frozenset(self.tools.replace(",", " ").split())

    def agent_engine_tokens(self) -> list[str]:
        """`agent_engines` split into lower-case words, in order."""
        return self.agent_engines.replace(",", " ").lower().split()

    @field_validator("cache_dir", "download_dir")
    @classmethod
    def expand_user_paths(cls, value: Path | None) -> Path | None:
        return value.expanduser() if value is not None else None

    def cache_path(self) -> Path:
        self.cache_dir.mkdir(parents=True, exist_ok=True)
        return self.cache_dir / "cache.sqlite"


settings = Settings()
