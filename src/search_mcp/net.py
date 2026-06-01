"""Outbound-network helpers: optional proxy resolution.

A single place that answers "should this outbound request go through a proxy,
and how?" so every HTTP engine, the browser pool, and the fetcher stay
consistent. Fully opt-in — when no proxy is configured every helper returns an
empty/None value and callers behave exactly as before.

The proxy is read through :mod:`keystore`, so it honours the same precedence as
API keys (``SEARCH_MCP_PROXY`` env var > admin-saved ``config.json`` > none) and
hot-reloads when saved in the admin UI. It may carry credentials
(``http://user:pass@host:port``), so it is treated as a secret (masked in the
admin UI, never logged).

Scoping: by default a configured proxy applies to ALL engines + the browser +
the fetcher. Set ``proxy_engines`` (a comma/space list, e.g. ``"google bing
zhihu"``) to route ONLY those engines through the proxy and keep the fast
defaults direct.
"""

from __future__ import annotations

from urllib.parse import urlparse

from .keystore import get_secret


def proxy_url() -> str | None:
    """The configured outbound proxy URL, or ``None``.

    Accepts ``http://``, ``https://`` and ``socks5://`` (with optional
    ``user:pass@``). ``SEARCH_MCP_PROXY`` env var wins over the admin-saved value.
    """
    return get_secret("proxy")


def _proxy_engine_scope() -> set[str]:
    raw = get_secret("proxy_engines") or ""
    return {e.strip().lower() for e in raw.replace(",", " ").split() if e.strip()}


def proxy_for(engine: str | None = None) -> str | None:
    """Proxy URL to use for ``engine`` (or the global proxy when ``engine`` is
    None / unscoped). Returns ``None`` when no proxy is set, or when a scope is
    configured and ``engine`` is not in it."""
    url = proxy_url()
    if not url:
        return None
    scope = _proxy_engine_scope()
    if engine and scope and engine.lower() not in scope:
        return None
    return url


def curl_proxy_kwargs(engine: str | None = None) -> dict:
    """kwargs to splat into ``curl_cffi`` ``AsyncSession(...)``: ``{"proxy": url}``
    when a proxy applies to ``engine``, else ``{}`` (no behaviour change)."""
    url = proxy_for(engine)
    return {"proxy": url} if url else {}


def playwright_proxy() -> dict | None:
    """Playwright ``proxy=`` launch dict (``{"server","username","password"}``)
    for the configured proxy, or ``None``. Browser traffic is global (not
    per-engine scoped), so this uses the unscoped proxy."""
    url = proxy_url()
    if not url:
        return None
    p = urlparse(url)
    if not p.hostname:
        return None
    server = f"{p.scheme}://{p.hostname}"
    if p.port:
        server += f":{p.port}"
    out: dict[str, str] = {"server": server}
    if p.username:
        out["username"] = p.username
    if p.password:
        out["password"] = p.password
    return out


__all__ = [
    "curl_proxy_kwargs",
    "playwright_proxy",
    "proxy_for",
    "proxy_url",
]
