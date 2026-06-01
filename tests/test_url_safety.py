"""Phase 3+4 tests for the SSRF guard (url_safety) and the config hardening.

We monkeypatch ``socket.getaddrinfo`` so the "allowed public host" cases never
touch the real network, and inject controlled IPs into the resolver to exercise
both the public-allow and private-block paths deterministically.
"""
import socket

import pytest

from search_mcp import config
from search_mcp.config import Settings
from search_mcp.url_safety import (
    UnsafeURLError,
    assert_ip_allowed,
    assert_url_allowed,
)


def _fake_getaddrinfo(*addresses):
    """Build a getaddrinfo stand-in that resolves any host to ``addresses``."""

    def _resolver(host, port, *args, **kwargs):
        out = []
        for addr in addresses:
            family = socket.AF_INET6 if ":" in addr else socket.AF_INET
            sockaddr = (addr, port or 0)
            out.append((family, socket.SOCK_STREAM, socket.IPPROTO_TCP, "", sockaddr))
        return out

    return _resolver


@pytest.fixture
def public_dns(monkeypatch):
    """Resolve every host to a single public IP, avoiding real DNS."""
    monkeypatch.setattr(socket, "getaddrinfo", _fake_getaddrinfo("93.184.216.34"))


# --- blocked schemes ------------------------------------------------------

@pytest.mark.parametrize("url", ["file:///etc/passwd", "ftp://x/", "gopher://x/", "data:text/plain,hi"])
def test_rejects_non_http_schemes(url):
    with pytest.raises(UnsafeURLError):
        assert_url_allowed(url)


# --- blocked IP literals (no DNS needed) ----------------------------------

@pytest.mark.parametrize(
    "url",
    [
        "http://169.254.169.254/latest/meta-data/",  # cloud metadata
        "http://127.0.0.1:8080/",                     # loopback
        "http://10.0.0.1/",                            # RFC1918
        "http://192.168.1.1/",                         # RFC1918
        "http://172.16.0.1/",                          # RFC1918
        "http://[::1]/",                               # v6 loopback
        "http://0.0.0.0/",                             # unspecified
    ],
)
def test_rejects_blocked_ip_literals(url):
    with pytest.raises(UnsafeURLError):
        assert_url_allowed(url)


def test_rejects_metadata_host_via_dns(monkeypatch):
    """A normal-looking hostname that *resolves* to a blocked IP is rejected."""
    monkeypatch.setattr(socket, "getaddrinfo", _fake_getaddrinfo("169.254.169.254"))
    with pytest.raises(UnsafeURLError):
        assert_url_allowed("http://metadata.internal.example/")


def test_rejects_when_any_resolved_ip_is_blocked(monkeypatch):
    """Mixed A records: one public, one private -> must block."""
    monkeypatch.setattr(socket, "getaddrinfo", _fake_getaddrinfo("93.184.216.34", "10.1.2.3"))
    with pytest.raises(UnsafeURLError):
        assert_url_allowed("http://rebind.example/")


def test_dns_failure_fails_closed(monkeypatch):
    def _boom(*a, **k):
        raise socket.gaierror("no such host")

    monkeypatch.setattr(socket, "getaddrinfo", _boom)
    with pytest.raises(UnsafeURLError):
        assert_url_allowed("http://does-not-resolve.invalid/")


# --- allowed public hosts -------------------------------------------------

def test_allows_public_ip_literal():
    assert assert_url_allowed("http://93.184.216.34/") == "http://93.184.216.34/"


def test_allows_public_hostname(public_dns):
    url = "https://example.com/path?q=1"
    assert assert_url_allowed(url) == url


# --- assert_ip_allowed (redirect-hop checks) ------------------------------

def test_assert_ip_allowed_passes_public():
    assert assert_ip_allowed("93.184.216.34") is None


@pytest.mark.parametrize("ip", ["127.0.0.1", "169.254.169.254", "10.0.0.1", "::1"])
def test_assert_ip_allowed_rejects_blocked(ip):
    with pytest.raises(UnsafeURLError):
        assert_ip_allowed(ip)


# --- allow_private_hosts escape hatch -------------------------------------

@pytest.fixture
def allow_private(monkeypatch):
    monkeypatch.setattr(config.settings, "allow_private_hosts", True)


def test_allow_private_bypasses_url_check(allow_private):
    # Should NOT raise even though the target is loopback; and no DNS happens.
    assert assert_url_allowed("http://127.0.0.1:8080/admin") == "http://127.0.0.1:8080/admin"


def test_allow_private_bypasses_metadata(allow_private):
    url = "http://169.254.169.254/latest/meta-data/"
    assert assert_url_allowed(url) == url


def test_allow_private_bypasses_ip_check(allow_private):
    assert assert_ip_allowed("10.0.0.1") is None


def test_allow_private_still_rejects_bad_scheme(allow_private):
    # The escape hatch is about destinations, not protocols.
    with pytest.raises(UnsafeURLError):
        assert_url_allowed("file:///etc/passwd")


# --- config hardening -----------------------------------------------------

def test_settings_rejects_zero_rate_limit():
    from pydantic import ValidationError

    with pytest.raises(ValidationError):
        Settings(rate_limit_per_minute=0)


def test_settings_rejects_zero_fetch_rate_limit():
    from pydantic import ValidationError

    with pytest.raises(ValidationError):
        Settings(fetch_rate_limit_per_minute=0)


def test_settings_rejects_negative_rate_limit():
    from pydantic import ValidationError

    with pytest.raises(ValidationError):
        Settings(rate_limit_per_minute=-5)


def test_new_safety_settings_defaults():
    s = Settings()
    assert s.allow_private_hosts is False
    assert s.document_root is None
    assert s.max_response_bytes == 25_000_000
    assert s.max_pdf_pages == 200
    assert s.max_document_chars == 2_000_000
