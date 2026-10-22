from __future__ import annotations

import asyncio
import logging
import random
import sys
import time
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from typing import TYPE_CHECKING, Any

from .config import settings
from .net import playwright_proxy

if TYPE_CHECKING:
    from playwright.async_api import BrowserContext, Page

log = logging.getLogger(__name__)


def async_playwright() -> Any:
    """Playwright's ``async_playwright()``, imported when a browser is first
    started rather than with this module: Playwright and greenlet are a large
    share of what a fresh install loads, and many servers never render a page."""
    from playwright.async_api import async_playwright as start

    return start()

# Cookie names that signal a completed interactive login (polled in
# BrowserPool.login). ``z_c0`` is Zhihu's auth cookie; the rest are common
# session-cookie names so the flow generalises to other sites.
_AUTH_COOKIE_NAMES = frozenset(
    {"z_c0", "sessdata", "sessionid", "sessionid_ss", "sid"}
)

# Chromium navigation errors that are worth a single immediate retry: they are
# transport-level hiccups (especially common behind a proxy) rather than a real
# "this page is broken" failure.
_TRANSIENT_NAV_MARKERS = (
    "ERR_TUNNEL_CONNECTION_FAILED",
    "ERR_TIMED_OUT",
    "ERR_CONNECTION_RESET",
    "ERR_CONNECTION_CLOSED",
    "ERR_NETWORK_CHANGED",
)


def _launch_proxy_kwargs() -> dict:
    """Playwright launch kwargs carrying the configured proxy, or ``{}``.

    Pure (no browser) so it is unit-testable: returns ``{"proxy": {...}}`` only
    when :func:`net.playwright_proxy` resolves a proxy, else an empty dict so we
    never pass ``proxy=None`` (which would still route through Playwright's proxy
    machinery). Never log the returned dict — it may carry credentials.
    """
    proxy = playwright_proxy()
    return {"proxy": proxy} if proxy else {}


def _is_transient_nav_error(msg: str) -> bool:
    """True when a navigation error message is a transient transport failure
    worth retrying once. Pure string check, unit-testable without a browser."""
    if not msg:
        return False
    return any(marker in msg for marker in _TRANSIENT_NAV_MARKERS)


def brief_error(exc: BaseException) -> str:
    """An exception's message without Playwright's call log.

    Playwright appends a multi-line "Call log:" to every error. It repeats the
    URL the first line already names, and it lands in text a model reads: the
    `errors` map of a search and the message of a failed `fetch`.
    """
    return str(exc).split("\nCall log:", 1)[0].strip()


class BrowserUnavailableError(RuntimeError):
    """The Chromium browser binary is not installed.

    Raised instead of Playwright's raw launch error so every downstream
    surface (engine errors, fetch errors, gate hints) carries an actionable
    install command rather than a stack trace. HTTP-only search and fetching
    are unaffected.
    """


class BrowserLaunchError(RuntimeError):
    """The driver or browser did not start within ``browser_launch_timeout``.

    A stalled launch (a flaky proxy, VPN or TUN dropping the Playwright driver
    handshake, or a wedged driver) is a transport hiccup, not a missing binary.
    So this is not a ``BrowserUnavailableError``: the gate hints must not
    answer a network blip with "install Chromium". The pool tries again after
    ``browser_launch_cooldown``, and HTTP-only search and fetching keep working
    meanwhile.
    """


# The ONE canonical install instruction — every surface (engine gate hints,
# fetch errors) must show these exact commands; a hand-written variant like
# a bare `playwright install chromium` does not work on uvx installs.
BROWSER_INSTALL_HINT = (
    "Install the browser with `uv run playwright install chromium` (source "
    "checkout) or `uvx --from free-search-mcp playwright install chromium` "
    "(uvx/PyPI install), then restart the server."
)

_MISSING_BROWSER_HINT = (
    "Playwright's Chromium browser is not installed, so browser-rendered "
    "engines and JS-heavy fetches are unavailable (HTTP-only search and "
    "fetching keep working). " + BROWSER_INSTALL_HINT
)


def _launch_timeout_hint() -> str:
    """Message for a launch that ran past ``browser_launch_timeout``.

    Kept apart from ``_MISSING_BROWSER_HINT`` because the remedy differs: a
    timeout means retry or fix the network, never "install Chromium".
    """
    return (
        "The browser did not start within "
        f"{settings.browser_launch_timeout:.0f}s. This is usually a stalled "
        "proxy, VPN or TUN, or a wedged Playwright driver, not a missing "
        "browser. HTTP-only search and fetching keep working, and the browser "
        f"is tried again after {settings.browser_launch_cooldown:.0f}s."
    )


# Substrings Playwright's launch error always carries when the browser
# BINARY is missing (as opposed to a crash or a bad flag).
_MISSING_BROWSER_MARKERS = ("executable doesn't exist", "playwright install")


def _is_missing_browser_error(msg: str) -> bool:
    """True when a launch failure means the browser binary was never
    downloaded. Pure string check, unit-testable without a browser."""
    if not msg:
        return False
    low = msg.lower()
    return any(marker in low for marker in _MISSING_BROWSER_MARKERS)

# Anti-detection script borrowed from noapi-google-search-mcp's playbook:
# disable webdriver flag, fake plugins/languages, skip Chrome runtime check.
_STEALTH_SCRIPT = """
Object.defineProperty(navigator, 'webdriver', { get: () => undefined });
Object.defineProperty(navigator, 'languages', { get: () => ['en-US', 'en'] });
Object.defineProperty(navigator, 'plugins', { get: () => [1, 2, 3, 4, 5] });
window.chrome = { runtime: {} };
const orig = window.navigator.permissions.query;
window.navigator.permissions.query = (p) => (
    p.name === 'notifications'
      ? Promise.resolve({ state: Notification.permission })
      : orig(p)
);
"""

# Seconds to wait for a half-built context to close, or a driver to stop.
_TEARDOWN_TIMEOUT = 5.0


async def _close_manager(manager) -> None:
    """Stop a Playwright driver whose ``start()`` never returned.

    The manager may already own the node process by then, and only the manager
    can stop it: the ``Playwright`` handle ``start()`` would have returned does
    not exist yet. Bounded, and silent when there is nothing to stop.
    """
    try:
        await asyncio.wait_for(manager.__aexit__(None, None, None), _TEARDOWN_TIMEOUT)
    except Exception:
        log.debug("playwright manager exit after a stalled start failed")

# Common desktop viewports — pick one per session to break up the trivially
# constant-1366x800 fingerprint without straying into freakish dimensions.
_VIEWPORTS = [
    (1366, 768),
    (1440, 900),
    (1536, 864),
    (1680, 1050),
]


class BrowserPool:
    """One shared persistent BrowserContext, semaphore-bounded page concurrency.

    Sharing the context keeps cookies and session storage across requests, which
    lets sites like Bing pass us through after the first warmup challenge. We
    use ``launch_persistent_context`` so the same profile (cookies, storage,
    HSTS list, etc.) survives across server restarts on disk.
    """

    def __init__(self) -> None:
        self._playwright = None
        # ``launch_persistent_context`` returns a BrowserContext directly; there
        # is no separate Browser handle to track.
        self._ctx: BrowserContext | None = None
        self._page_sema = asyncio.Semaphore(settings.browser_pool_size)
        self._lock = asyncio.Lock()
        # Set once a launch failed because the browser binary is missing.
        # Installing it requires a restart, so subsequent calls fail fast
        # instead of re-paying a ~1s driver start/stop per attempt.
        self._unavailable_reason: str | None = None
        # ``time.monotonic()`` of the last launch timeout, or None. Unlike
        # ``_unavailable_reason`` it expires after ``browser_launch_cooldown``,
        # so a network blip heals by itself while a missing binary stays
        # missing until ``shutdown()``.
        self._launch_failed_at: float | None = None

    @property
    def known_unavailable(self) -> bool:
        """True once a launch has failed for want of a browser binary. False
        means "not known to be missing", not "present" — nothing is probed."""
        return self._unavailable_reason is not None

    def _launch_args(self) -> list[str]:
        args = [
            "--disable-blink-features=AutomationControlled",
            "--disable-dev-shm-usage",
        ]
        # ``--no-sandbox`` is itself a fingerprint marker (no real desktop
        # Chrome ships with it). Only keep it on Linux/containers where the
        # sandbox often can't run anyway.
        if sys.platform != "darwin":
            args.append("--no-sandbox")
        return args

    async def _teardown_driver(self) -> None:
        """Close any half-built context and stop the Playwright driver.

        Both are reset to ``None`` so the next ``_ensure`` starts clean instead
        of leaking a driver (and its node process) per failed attempt. Both
        awaits are bounded, because a driver that just stalled is the one whose
        ``close`` or ``stop`` may block too, and an unbounded teardown would
        bring back the hang that holds the lock forever.
        """
        if self._ctx is not None:
            try:
                await asyncio.wait_for(self._ctx.close(), _TEARDOWN_TIMEOUT)
            except Exception:
                log.debug("ctx close during teardown failed")
            self._ctx = None
        if self._playwright is not None:
            try:
                await asyncio.wait_for(self._playwright.stop(), _TEARDOWN_TIMEOUT)
            except Exception:
                log.debug("playwright stop during teardown failed")
            self._playwright = None

    async def _ensure(self) -> BrowserContext:
        async with self._lock:
            if self._ctx is not None:
                return self._ctx
            if self._unavailable_reason is not None:
                raise BrowserUnavailableError(self._unavailable_reason)
            # After a launch timeout, fail fast for the cooldown, so a burst of
            # concurrent fetches does not each wait out the full timeout again.
            if self._launch_failed_at is not None:
                if (
                    time.monotonic() - self._launch_failed_at
                    < settings.browser_launch_cooldown
                ):
                    raise BrowserLaunchError(_launch_timeout_hint())
                self._launch_failed_at = None

            timeout = settings.browser_launch_timeout
            # Both awaits below run under self._lock. Unbounded, a driver start
            # or launch that hangs (a proxy, VPN or TUN dropping the handshake)
            # holds the lock forever, and every later fetch in the process
            # waits behind it until the MCP client gives up. Bounded, the
            # stall costs one failed request.
            manager = async_playwright()
            try:
                self._playwright = await asyncio.wait_for(manager.start(), timeout=timeout)
            except TimeoutError as e:
                # The driver handshake hung. Treat it like a stalled launch:
                # stop what the manager started, cool down, and raise the
                # network hint.
                await _close_manager(manager)
                self._launch_failed_at = time.monotonic()
                raise BrowserLaunchError(_launch_timeout_hint()) from e

            user_data_dir = str(settings.cache_dir / "browser_profile")
            settings.cache_dir.mkdir(parents=True, exist_ok=True)

            width, height = random.choice(_VIEWPORTS)
            common_kwargs = dict(
                user_data_dir=user_data_dir,
                headless=settings.browser_headless,
                args=self._launch_args(),
                user_agent=settings.user_agent,
                locale="en-US",
                timezone_id="America/Los_Angeles",
                viewport={"width": width, "height": height},
                extra_http_headers={"Accept-Language": settings.accept_language},
                # Opt-in proxy: empty dict (no `proxy` key) when none configured,
                # so behaviour is byte-for-byte unchanged without a proxy.
                **_launch_proxy_kwargs(),
            )

            async def _launch() -> BrowserContext:
                # Prefer a real installed Chrome (better fingerprint than the
                # bundled Chromium); fall back transparently when it's absent.
                try:
                    return await self._playwright.chromium.launch_persistent_context(
                        channel="chrome",
                        **common_kwargs,
                    )
                except Exception as e:
                    log.warning(
                        "real Chrome not found, using bundled Chromium: %s", e
                    )
                    return await self._playwright.chromium.launch_persistent_context(
                        **common_kwargs,
                    )

            # If the launch (or the stealth init script) fails, the started
            # driver must not leak, or every later _ensure starts another and
            # node processes pile up. Tear it down so the next call starts
            # clean.
            try:
                self._ctx = await asyncio.wait_for(_launch(), timeout=timeout)
                try:
                    await self._ctx.add_init_script(_STEALTH_SCRIPT)
                except Exception:
                    # Context launched but stealth wiring failed: tear it down
                    # so the next _ensure rebuilds it.
                    await self._teardown_driver()
                    raise
            except TimeoutError as e:
                # The launch stalled past the ceiling. That is transient, not a
                # missing binary: a network hint and a short cooldown, never
                # "install Chromium".
                await self._teardown_driver()
                self._launch_failed_at = time.monotonic()
                raise BrowserLaunchError(_launch_timeout_hint()) from e
            except Exception as e:
                # Launch failed entirely (or stealth re-raised): stop the driver
                # and reset so we don't leak it across retries.
                await self._teardown_driver()
                if _is_missing_browser_error(str(e)):
                    self._unavailable_reason = _MISSING_BROWSER_HINT
                    raise BrowserUnavailableError(self._unavailable_reason) from e
                raise
            return self._ctx

    @asynccontextmanager
    async def page(self) -> AsyncIterator[Page]:
        async with self._page_sema:
            ctx = await self._ensure()
            page = await ctx.new_page()
            page.set_default_timeout(int(settings.fetch_timeout * 1000))
            try:
                yield page
            finally:
                try:
                    await page.close()
                except Exception:
                    pass

    async def fetch_html(self, url: str, wait_selector: str | None = None) -> tuple[str, str]:
        async with self.page() as page:
            try:
                await page.goto(url, wait_until="domcontentloaded")
            except Exception as e:
                # Transient transport failures (common behind a flaky proxy) get
                # one immediate retry after a short backoff; anything else, or a
                # second failure, propagates to the caller's graceful handling.
                if not _is_transient_nav_error(str(e)):
                    raise
                log.debug("transient nav error, retrying once: %s", e)
                await asyncio.sleep(random.uniform(0.5, 1.2))
                await page.goto(url, wait_until="domcontentloaded")
            if wait_selector:
                try:
                    await page.wait_for_selector(wait_selector, timeout=8000)
                except Exception:
                    pass
            await asyncio.sleep(random.uniform(0.4, 1.1))
            html = await page.content()
            title = await page.title()
            return title, html

    async def launch(self) -> None:
        """Start the browser now, so the first page does not wait for it.

        A failure is not raised: the page that needs the browser meets the
        same memoised state (missing binary, launch cooldown) and reports it.
        """
        try:
            await self._ensure()
        except Exception as e:
            log.debug("browser launch ahead of use failed: %s", brief_error(e))

    async def warmup(self, url: str) -> None:
        """Open a URL once to seed cookies for an origin, ignoring failures."""
        try:
            async with self.page() as page:
                await page.goto(url, wait_until="domcontentloaded")
                await asyncio.sleep(random.uniform(0.3, 0.8))
        except Exception as e:
            log.debug("warmup %s failed: %s", url, e)

    async def login(self, url: str, *, timeout_ms: int = 180000) -> bool:
        """Open a HEADED login window for an interactive sign-in (e.g. Zhihu).

        Requires a desktop session (a visible browser window appears). Uses the
        SAME persistent user-data-dir as the pool so cookies are written to disk
        and later headless searches reuse the session. Forces ``headless=False``
        for this window even when the pool default is headless.

        Navigates to ``url`` and waits (best-effort) until the user finishes
        signing in — detected as a navigation away from any ``/signin`` or
        ``/login`` path — or until ``timeout_ms`` elapses. Returns ``True`` on
        success, ``False`` on failure.
        """
        manager = async_playwright()
        playwright = None
        ctx: BrowserContext | None = None
        timeout = settings.browser_launch_timeout
        try:
            # Bounded like _ensure(), so a stalled driver or launch cannot hang
            # the login command forever.
            playwright = await asyncio.wait_for(manager.start(), timeout=timeout)
            user_data_dir = str(settings.cache_dir / "browser_profile")
            settings.cache_dir.mkdir(parents=True, exist_ok=True)

            width, height = random.choice(_VIEWPORTS)
            common_kwargs = dict(
                user_data_dir=user_data_dir,
                headless=False,  # forced visible window for interactive login
                args=self._launch_args(),
                user_agent=settings.user_agent,
                locale="en-US",
                timezone_id="America/Los_Angeles",
                viewport={"width": width, "height": height},
                extra_http_headers={"Accept-Language": settings.accept_language},
                **_launch_proxy_kwargs(),
            )

            async def _launch_login() -> BrowserContext:
                try:
                    return await playwright.chromium.launch_persistent_context(
                        channel="chrome", **common_kwargs
                    )
                except Exception as e:
                    log.warning("real Chrome not found for login, using Chromium: %s", e)
                    return await playwright.chromium.launch_persistent_context(
                        **common_kwargs
                    )

            ctx = await asyncio.wait_for(_launch_login(), timeout=timeout)
            try:
                await ctx.add_init_script(_STEALTH_SCRIPT)
            except Exception:
                log.debug("login add_init_script failed")

            page = await ctx.new_page()
            await page.goto(url, wait_until="domcontentloaded")
            # Wait for the user to actually authenticate. A URL change is NOT a
            # reliable signal — sites like Zhihu show a login MODAL while the URL
            # stays at "/" — so poll for an auth cookie, and also stop early if
            # the user closes the window. Cookies in the persistent context are
            # flushed to disk live, so they survive whichever way the wait ends.
            poll_s = 1.0
            elapsed_ms = 0
            while elapsed_ms < timeout_ms:
                if page.is_closed():
                    break
                try:
                    cookies = await ctx.cookies()
                except Exception:
                    cookies = []
                if any(
                    (c.get("name") or "").lower() in _AUTH_COOKIE_NAMES
                    for c in cookies
                ):
                    log.info("interactive login: auth cookie detected")
                    break
                await asyncio.sleep(poll_s)
                elapsed_ms += int(poll_s * 1000)
            return True
        except Exception as e:
            log.warning("interactive login failed: %s", e)
            return False
        finally:
            # Bounded too: after a stall, close and stop can hang on the same
            # driver. The manager's exit is what playwright.stop() calls, and
            # it also covers a start() that never returned.
            if ctx is not None:
                try:
                    await asyncio.wait_for(ctx.close(), _TEARDOWN_TIMEOUT)
                except Exception:
                    log.debug("login ctx close failed")
            await _close_manager(manager)

    async def shutdown(self) -> None:
        async with self._lock:
            if self._ctx:
                try:
                    await asyncio.wait_for(
                        self._ctx.close(), timeout=settings.browser_launch_timeout
                    )
                except Exception:
                    log.exception("context close failed")
                self._ctx = None
            if self._playwright:
                try:
                    await asyncio.wait_for(
                        self._playwright.stop(),
                        timeout=settings.browser_launch_timeout,
                    )
                except Exception:
                    log.exception("playwright stop failed")
                self._playwright = None
            # A shutdown is the "restart" the install hint asks for, so the next
            # launch starts clean: no missing-binary memo, no cooldown.
            self._unavailable_reason = None
            self._launch_failed_at = None


pool = BrowserPool()
