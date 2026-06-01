"""Console entry point for an interactive browser login.

``search-mcp-login [zhihu|<url>]`` opens a real browser window so you can log in
to a site that the headless engines can't authenticate to with an API key (e.g.
zhihu). The persisted session is then reused by the browser pool.

Usage::

    search-mcp-login            # defaults to zhihu
    search-mcp-login zhihu
    search-mcp-login https://example.com/login
"""

from __future__ import annotations

import asyncio
import sys

# Friendly aliases -> the URL the login flow opens.
_ALIASES: dict[str, str] = {"zhihu": "https://www.zhihu.com"}


def _resolve(arg: str) -> str:
    """Map a CLI argument to a login URL (alias or a passed-through URL)."""
    return _ALIASES.get(arg, arg)


def main() -> None:
    args = sys.argv[1:]
    target = args[0] if args else "zhihu"
    url = _resolve(target)

    print("a browser window will open; log in, it auto-closes")

    # Import lazily so importing this module never pulls in Playwright.
    from .browser import pool

    ok = asyncio.run(pool.login(url))
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
