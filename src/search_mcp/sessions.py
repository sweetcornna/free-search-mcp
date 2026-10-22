"""Long-lived curl_cffi sessions, so a request does not re-pay connection setup.

Every engine and the fetch path used to open a fresh ``AsyncSession`` per
request, which throws the connection away and pays DNS, TCP and TLS again on
the next one. Measured 2026-10-22 through a fake-IP tunnel, a fresh session
against a reused one: Bing 1.64 s vs 0.70 s, AnySearch 0.91 s vs 0.25 s,
Wikipedia 1.23 s vs 0.40 s. Every search sits on that path several times.

:class:`AsyncSession` is a drop-in for the ``async with AsyncSession(**kw) as
client:`` blocks the modules already have, and it keeps their name so tests
can go on monkeypatching ``<module>.AsyncSession``. The constructor kwargs that
fix what a connection IS (impersonation profile, proxy, TLS verification)
select a shared session; the rest (headers, cookies, timeout, redirects)
become defaults for each request made through the block.

Cookies live exactly as long as the ``async with`` block, as they did when the
block owned its session. The shared sessions keep no cookies; each block has
its own jar, seeded with the constructor's cookies and topped up with what each
response sets, so the hops of a manual redirect still see the cookie the
previous hop set and the next block starts empty. A block reads cookies from
the final response of each request: with ``allow_redirects=True`` curl carries
the ones set on intermediate hops within that request, but they do not reach a
later request in the block. Code that reads back the cookies a response set
(Bing's warm-up) keeps a real ``curl_cffi`` session of its own.

A session is bound to the event loop it was made on. The server runs one loop;
when the running loop changes (each test has its own) the old sessions are
dropped rather than closed, since their loop is already gone.
"""

from __future__ import annotations

import asyncio
from typing import Any
from urllib.parse import urlsplit

from curl_cffi.requests import AsyncSession as _CurlSession
from curl_cffi.requests import Cookies

# Kwargs that change the connection itself. Two blocks that differ in any of
# them must not share a connection: reusing one opened under another TLS
# fingerprint or another proxy would send the request as someone else.
_IDENTITY = ("impersonate", "proxy", "proxies", "verify", "ja3", "akamai", "extra_fp")

# curl handles per shared session, which is how many of its requests can be in
# flight at once. curl_cffi's default of 10 is below what a search fan-out plus
# a fetch_batch can ask of the one Chrome profile most callers share.
_MAX_CLIENTS = 32

_loop: asyncio.AbstractEventLoop | None = None
_sessions: dict[tuple[tuple[str, str], ...], _CurlSession] = {}


def _shared(identity: dict[str, Any]) -> _CurlSession:
    global _loop
    loop = asyncio.get_running_loop()
    if loop is not _loop:
        _sessions.clear()
        _loop = loop
    key = tuple(sorted((k, repr(v)) for k, v in identity.items()))
    session = _sessions.get(key)
    if session is None:
        session = _CurlSession(**identity, max_clients=_MAX_CLIENTS, discard_cookies=True)
        _sessions[key] = session
    return session


def reset() -> None:
    """Forget every shared session (tests that count connections)."""
    global _loop
    _sessions.clear()
    _loop = None


class AsyncSession:
    """``async with AsyncSession(**kw) as client`` over a shared, long-lived session."""

    def __init__(self, **kwargs: Any) -> None:
        self._identity = {k: kwargs.pop(k) for k in _IDENTITY if k in kwargs}
        self._jar = Cookies(kwargs.pop("cookies", None))
        self._defaults = kwargs
        self._session: _CurlSession | None = None

    @property
    def cookies(self) -> Cookies:
        return self._jar

    def _keep_cookies(self, resp: Any) -> None:
        host = urlsplit(resp.url).hostname or ""
        for cookie in resp.cookies.jar:
            if not cookie.domain:
                # No Domain attribute: the cookie belongs to the host that set
                # it alone (domain_specified stays False, so no subdomains).
                cookie.domain = host
            self._jar.jar.set_cookie(cookie)

    async def __aenter__(self) -> AsyncSession:
        self._session = _shared(self._identity)
        return self

    async def __aexit__(self, *exc: object) -> None:
        self._session = None

    def _request_kwargs(self, kwargs: dict[str, Any]) -> dict[str, Any]:
        merged = {**self._defaults, **kwargs}
        base, extra = self._defaults.get("headers"), kwargs.get("headers")
        if base and extra:
            merged["headers"] = {**base, **extra}
        return merged

    def _client(self) -> _CurlSession:
        if self._session is None:
            raise RuntimeError("use AsyncSession inside `async with`")
        return self._session

    async def request(self, method: str, url: str, **kwargs: Any) -> Any:
        client = self._client()
        merged = self._request_kwargs(kwargs)
        extra = merged.pop("cookies", None)
        if extra:
            for cookie in Cookies(extra).jar:
                self._jar.jar.set_cookie(cookie)
        resp = await client.request(method, url, cookies=self._jar, **merged)
        self._keep_cookies(resp)
        return resp

    async def get(self, url: str, **kwargs: Any) -> Any:
        return await self.request("GET", url, **kwargs)

    async def post(self, url: str, **kwargs: Any) -> Any:
        return await self.request("POST", url, **kwargs)


__all__ = ["AsyncSession", "reset"]
