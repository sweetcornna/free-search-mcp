"""Each pytest-asyncio test gets a fresh event loop, but the BrowserPool
caches a Playwright instance bound to the loop where it was first created.
Shutting it down between tests keeps the pool from carrying a dead loop into
the next test.
"""
import pytest


@pytest.fixture(autouse=True)
async def _reset_browser_pool():
    yield
    from search_mcp.browser import pool
    await pool.shutdown()
