import asyncio
import logging
import random
from contextlib import asynccontextmanager
from typing import AsyncIterator

from playwright.async_api import Browser, BrowserContext, Page, async_playwright

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


class BrowserPool:
    """One shared persistent BrowserContext, semaphore-bounded page concurrency.

    Sharing the context keeps cookies and session storage across requests, which
    lets sites like Bing pass us through after the first warmup challenge.
    """

    def __init__(self) -> None:
        self._playwright = None
        self._browser: Browser | None = None
        self._ctx: BrowserContext | None = None
        self._page_sema = asyncio.Semaphore(settings.browser_pool_size)
        self._lock = asyncio.Lock()

    async def _ensure(self) -> BrowserContext:
        async with self._lock:
            if self._ctx and self._browser and self._browser.is_connected():
                return self._ctx
            self._playwright = await async_playwright().start()
            self._browser = await self._playwright.chromium.launch(
                headless=settings.browser_headless,
                args=[
                    "--disable-blink-features=AutomationControlled",
                    "--disable-dev-shm-usage",
                    "--no-sandbox",
                ],
            )
            self._ctx = await self._browser.new_context(
                user_agent=settings.user_agent,
                locale="en-US",
                timezone_id="America/Los_Angeles",
                viewport={"width": 1366, "height": 800},
                extra_http_headers={"Accept-Language": settings.accept_language},
            )
            await self._ctx.add_init_script(_STEALTH_SCRIPT)
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
                    pass
                self._ctx = None
            if self._browser:
                try:
                    await self._browser.close()
                except Exception:
                    log.exception("browser close failed")
                self._browser = None
            if self._playwright:
                await self._playwright.stop()
                self._playwright = None


pool = BrowserPool()
