import asyncio
import logging
import random
import sys
from contextlib import asynccontextmanager
from typing import AsyncIterator

from playwright.async_api import BrowserContext, Page, async_playwright

from .config import settings

log = logging.getLogger(__name__)

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

    async def _ensure(self) -> BrowserContext:
        async with self._lock:
            if self._ctx is not None:
                return self._ctx
            self._playwright = await async_playwright().start()

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
            )

            # Prefer a real installed Chrome (better fingerprint than bundled
            # Chromium). Fall back transparently when Chrome isn't installed.
            #
            # If BOTH launch attempts (or the stealth init script) fail we must
            # not leak the started Playwright driver — otherwise it is silently
            # re-started on every subsequent _ensure call, piling up zombie node
            # processes. Tear it down and reset state so the next call retries
            # from a clean slate.
            try:
                try:
                    self._ctx = await self._playwright.chromium.launch_persistent_context(
                        channel="chrome",
                        **common_kwargs,
                    )
                except Exception as e:
                    log.warning(
                        "real Chrome not found, using bundled Chromium: %s", e
                    )
                    self._ctx = await self._playwright.chromium.launch_persistent_context(
                        **common_kwargs,
                    )
                try:
                    await self._ctx.add_init_script(_STEALTH_SCRIPT)
                except Exception:
                    # Context launched but stealth wiring failed: close the
                    # half-built context so the next _ensure rebuilds it.
                    try:
                        await self._ctx.close()
                    except Exception:
                        log.debug("ctx close after add_init_script failure failed")
                    self._ctx = None
                    raise
            except Exception:
                # Launch failed entirely (or stealth re-raised): stop the driver
                # and reset so we don't leak it across retries.
                if self._playwright is not None:
                    try:
                        await self._playwright.stop()
                    except Exception:
                        log.debug("playwright stop after launch failure failed")
                    self._playwright = None
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

    async def warmup(self, url: str) -> None:
        """Open a URL once to seed cookies for an origin, ignoring failures."""
        try:
            async with self.page() as page:
                await page.goto(url, wait_until="domcontentloaded")
                await asyncio.sleep(random.uniform(0.3, 0.8))
        except Exception as e:
            log.debug("warmup %s failed: %s", url, e)

    async def shutdown(self) -> None:
        async with self._lock:
            if self._ctx:
                try:
                    await self._ctx.close()
                except Exception:
                    log.exception("context close failed")
                self._ctx = None
            if self._playwright:
                await self._playwright.stop()
                self._playwright = None


pool = BrowserPool()
