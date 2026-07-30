"""The SSRF guard under a proxy.

The guard used to resolve the target hostname locally on every request. Through
a proxy that address is not the one connected to, and in the deployments where a
proxy is most likely to exist the local answer is either unavailable or a
blackhole — so the guard refused every fetch and took the server down.

These tests pin the split: the DNS-free layers (scheme, address literal,
internal hostname) hold ALWAYS, and only the resolve-and-check layer stands down
when a proxy is in play.
"""
from __future__ import annotations

import asyncio
import socket

import pytest

from search_mcp import url_safety
from search_mcp.url_safety import (
    UnsafeURLError,
    assert_url_allowed,
    assert_url_allowed_async,
)


@pytest.fixture(params=["sync", "async"])
def guard(request):
    if request.param == "sync":
        return assert_url_allowed

    def _run(url):
        return asyncio.run(assert_url_allowed_async(url))

    return _run


@pytest.fixture
def proxied(monkeypatch):
    """Pretend an outbound proxy is configured."""
    monkeypatch.setattr(url_safety.net, "proxy_url", lambda: "http://127.0.0.1:7890")


@pytest.fixture
def dead_dns(monkeypatch):
    """Proxy-only egress: this host cannot resolve public names at all."""
    def _boom(*a, **k):
        raise socket.gaierror(8, "nodename nor servname provided")

    monkeypatch.setattr(socket, "getaddrinfo", _boom)


@pytest.fixture
def poisoned_dns(monkeypatch):
    """A resolver that answers blocked names with a blackhole address."""
    def _resolver(host, port, *a, **k):
        return [(socket.AF_INET, socket.SOCK_STREAM, socket.IPPROTO_TCP, "",
                 ("0.0.0.0", port or 0))]

    monkeypatch.setattr(socket, "getaddrinfo", _resolver)


# --- the regression: a proxy must not take the server down ----------------

def test_proxied_fetch_survives_dead_local_dns(guard, proxied, dead_dns):
    assert guard("https://www.google.com/search?q=x")


def test_proxied_fetch_survives_poisoned_local_dns(guard, proxied, poisoned_dns):
    assert guard("https://www.google.com/search?q=x")


def test_unproxied_fetch_still_fails_closed_on_dead_dns(guard, dead_dns):
    """Without a proxy this process IS the resolver, so the strict behaviour
    is kept — the connection could not have succeeded anyway."""
    with pytest.raises(UnsafeURLError):
        guard("https://www.google.com/search?q=x")


def test_unproxied_fetch_still_blocks_a_private_resolution(guard, poisoned_dns):
    with pytest.raises(UnsafeURLError):
        guard("https://internal.example.com/")


def test_always_mode_restores_strict_resolution_under_proxy(
    guard, proxied, dead_dns, monkeypatch
):
    from search_mcp.config import settings

    monkeypatch.setattr(settings, "ssrf_resolve_addresses", "always")
    with pytest.raises(UnsafeURLError):
        guard("https://www.google.com/search?q=x")


def test_never_mode_skips_resolution_entirely(guard, dead_dns, monkeypatch):
    from search_mcp.config import settings

    monkeypatch.setattr(settings, "ssrf_resolve_addresses", "never")
    assert guard("https://www.google.com/search?q=x")
    # ...but the DNS-free layers still hold.
    for url in ("http://127.0.0.1/", "http://metadata.google.internal/"):
        with pytest.raises(UnsafeURLError):
            guard(url)


# --- fake-IP VPN (Clash/Surge/sing-box TUN) --------------------------------
# Every hostname resolves into a tunnel range; the answer is a handle the
# tunnel translates, not a destination. This is what made a real user disable
# the guard outright rather than lose fetch/read_doc.


@pytest.fixture
def fake_ip_dns(monkeypatch):
    """Resolve EVERY host into 198.18.0.0/15 + a v6 ULA, as a TUN tunnel does."""
    counter = {"n": 0}

    def _resolver(host, port, *a, **k):
        counter["n"] += 1
        n = counter["n"] % 250 + 1
        return [
            (socket.AF_INET, socket.SOCK_STREAM, socket.IPPROTO_TCP, "",
             (f"198.18.0.{n}", port or 0)),
            (socket.AF_INET6, socket.SOCK_STREAM, socket.IPPROTO_TCP, "",
             (f"fdfe:dcba:9876::{n:x}", port or 0, 0, 0)),
        ]

    monkeypatch.setattr(socket, "getaddrinfo", _resolver)


def test_fake_ip_tunnel_does_not_block_public_hosts(guard, fake_ip_dns):
    assert guard("https://example.com/")
    assert guard("https://www.google.com/search?q=x")


@pytest.mark.parametrize(
    "url",
    [
        "http://127.0.0.1:8080/",
        "http://169.254.169.254/latest/meta-data/",
        "http://10.0.0.1/",
        "http://localhost/",
        "http://metadata.google.internal/v1/",
        "file:///etc/passwd",
    ],
)
def test_fake_ip_tunnel_still_blocks_the_dns_free_layers(guard, fake_ip_dns, url):
    """Standing layer 4 down must not open loopback or metadata."""
    with pytest.raises(UnsafeURLError):
        guard(url)


def test_fake_ip_detection_never_excuses_rfc1918(guard, monkeypatch):
    """A resolver mapping everything into 10.0.0.0/8 is NOT treated as a
    tunnel: auto-detection may only stand down for ranges a fake-IP tunnel
    actually uses, never for a real private network."""
    monkeypatch.setattr(
        socket,
        "getaddrinfo",
        lambda host, port=None, *a, **k: [
            (socket.AF_INET, socket.SOCK_STREAM, socket.IPPROTO_TCP, "",
             ("10.0.0.5", port or 0))
        ],
    )
    with pytest.raises(UnsafeURLError):
        guard("https://example.org/")


def test_honest_resolver_still_blocks_a_tunnel_range_result(guard, monkeypatch):
    """198.18.x is only excused when the CANARIES land there too. A resolver
    that answers honestly for canaries and 198.18.x for one host is describing
    a real reserved-range destination, and it stays blocked."""
    def _resolver(host, port=None, *a, **k):
        addr = "93.184.216.34" if host in ("example.com", "a.root-servers.net") \
            else "198.18.0.99"
        return [(socket.AF_INET, socket.SOCK_STREAM, socket.IPPROTO_TCP, "",
                 (addr, port or 0))]

    monkeypatch.setattr(socket, "getaddrinfo", _resolver)
    with pytest.raises(UnsafeURLError):
        guard("https://evil.example.net/")


def test_fake_ip_detection_is_off_in_always_mode(guard, fake_ip_dns, monkeypatch):
    from search_mcp.config import settings

    monkeypatch.setattr(settings, "ssrf_resolve_addresses", "always")
    with pytest.raises(UnsafeURLError):
        guard("https://example.com/")


# --- what must STILL be blocked when proxied ------------------------------
# Aiming a proxy at loopback or a metadata endpoint is worse than doing it
# directly: the proxy reads the credentials on the caller's behalf.

@pytest.mark.parametrize(
    "url",
    [
        "http://127.0.0.1:8080/",
        "http://[::1]/",
        "http://169.254.169.254/latest/meta-data/",
        "http://10.0.0.1/",
        "http://192.168.1.1/",
        "http://0.0.0.0/",
        "http://100.100.100.200/latest/meta-data/",  # Alibaba Cloud metadata
    ],
)
def test_address_literals_blocked_even_when_proxied(guard, proxied, dead_dns, url):
    with pytest.raises(UnsafeURLError):
        guard(url)


@pytest.mark.parametrize(
    "url",
    [
        "http://localhost:8080/",
        "http://metadata.google.internal/computeMetadata/v1/",
        "http://metadata.goog/",
        "http://instance-data/latest/meta-data/",
        "http://foo.internal/",
        "http://printer.local/",
        "http://db.corp/",
        "http://nas.lan/",
        "http://svc.home.arpa/",
        "http://app.localhost/",
    ],
)
def test_internal_hostnames_blocked_even_when_proxied(guard, proxied, dead_dns, url):
    with pytest.raises(UnsafeURLError):
        guard(url)


def test_internal_hostname_check_is_case_and_dot_insensitive(guard, proxied, dead_dns):
    for url in ("http://LOCALHOST/", "http://Metadata.Google.Internal./"):
        with pytest.raises(UnsafeURLError):
            guard(url)


@pytest.mark.parametrize("url", ["file:///etc/passwd", "gopher://x/"])
def test_scheme_check_holds_when_proxied(guard, proxied, dead_dns, url):
    with pytest.raises(UnsafeURLError):
        guard(url)


def test_ordinary_public_hostnames_are_not_caught_by_the_name_check(
    guard, proxied, dead_dns
):
    """The internal-name suffixes must not swallow real public hosts."""
    for url in [
        "https://example.com/",
        "https://api.internal-tools.com/",   # 'internal' is not the suffix
        "https://local.google.com/",          # 'local' is not the suffix
        "https://corp.example.org/",
    ]:
        assert guard(url)


# --- the new address ranges ------------------------------------------------

def test_alibaba_metadata_range_blocked_unproxied(guard, monkeypatch):
    """100.64.0.0/10 is CGNAT space that `ipaddress` reports as ordinary public
    address space, but it holds Alibaba Cloud's metadata endpoint."""
    monkeypatch.setattr(
        socket,
        "getaddrinfo",
        lambda host, port, *a, **k: [
            (socket.AF_INET, socket.SOCK_STREAM, socket.IPPROTO_TCP, "",
             ("100.100.100.200", port or 0))
        ],
    )
    with pytest.raises(UnsafeURLError):
        guard("https://evil.example.com/")


# --- bypasses that made the guard advisory -------------------------------


async def test_browser_fallback_cannot_bypass_the_guard(monkeypatch):
    """`fetch_page` swallowed the guard's rejection as if it were a transport
    error and handed the same URL to the Chromium render, which ran no check —
    so every blocked target was reachable by simply being unreachable over
    HTTP first."""
    from search_mcp import fetcher

    rendered: list[str] = []

    async def _boom_render(url, *a, **k):
        rendered.append(url)
        return "title", "<html><body>SECRET</body></html>"

    monkeypatch.setattr(fetcher.pool, "fetch_html", _boom_render)

    with pytest.raises(UnsafeURLError):
        await fetcher.fetch_page("http://169.254.169.254/latest/meta-data/")
    assert rendered == [], "browser render was reached for a blocked URL"


async def test_render_browser_is_guarded_too(monkeypatch):
    """render='browser' skips the HTTP attempt entirely, so it needs its own
    check rather than inheriting one from the branch it does not take."""
    from search_mcp import fetcher

    rendered: list[str] = []

    async def _render(url, *a, **k):
        rendered.append(url)
        return "t", "<html>x</html>"

    monkeypatch.setattr(fetcher.pool, "fetch_html", _render)

    with pytest.raises(UnsafeURLError):
        await fetcher.fetch_page("http://127.0.0.1:9000/", render="browser")
    assert rendered == []


async def test_cache_hit_cannot_bypass_the_guard(monkeypatch):
    """A cached body is still that URL's contents. The cache was read before
    any check, so anything fetched while the guard was permissive stayed
    retrievable after the setting was tightened."""
    from search_mcp import fetcher

    async def _cached(url, **k):
        return {"content": "SECRET", "title": "t"}

    monkeypatch.setattr(fetcher.cache, "get_page", _cached)

    # A public URL still serves from cache.
    ok = await fetcher.fetch_page("https://example.com/")
    assert ok.method == "cache"

    for blocked in (
        "http://169.254.169.254/latest/meta-data/",
        "http://metadata.google.internal/v1/",
        "http://127.0.0.1:8000/",
    ):
        with pytest.raises(UnsafeURLError):
            await fetcher.fetch_page(blocked)


def test_offline_guard_needs_no_dns(monkeypatch):
    """assert_url_allowed_offline must never resolve — it runs on cache hits."""
    from search_mcp.url_safety import assert_url_allowed_offline

    def _explode(*a, **k):
        raise AssertionError("offline guard performed a DNS lookup")

    monkeypatch.setattr(socket, "getaddrinfo", _explode)
    assert assert_url_allowed_offline("https://example.com/")
    with pytest.raises(UnsafeURLError):
        assert_url_allowed_offline("http://localhost/")


def test_public_addresses_still_allowed(guard, monkeypatch):
    monkeypatch.setattr(
        socket,
        "getaddrinfo",
        lambda host, port, *a, **k: [
            (socket.AF_INET, socket.SOCK_STREAM, socket.IPPROTO_TCP, "",
             ("93.184.216.34", port or 0))
        ],
    )
    assert guard("https://example.com/")
