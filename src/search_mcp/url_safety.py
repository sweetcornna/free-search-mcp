"""SSRF-guard helpers.

The MCP server fetches arbitrary user-supplied URLs. Without a guard, a caller
could point a tool at ``http://169.254.169.254/`` (cloud metadata), at
``http://127.0.0.1:.../`` (loopback services), or at any RFC1918 host reachable
from the server and exfiltrate internal data. ``assert_url_allowed`` resolves
the hostname to *every* A/AAAA address and rejects the request if any of them
land on a blocked range. Each redirect hop is independently re-validated by
``assert_url_allowed`` (re-resolving every A/AAAA record), which mitigates — but
does not fully eliminate — DNS rebinding: a residual TOCTOU window remains
because the HTTP client does its own connect-time DNS that is not pinned to the
validated addresses. ``assert_ip_allowed`` is provided for callers that have
already resolved an IP literal and want to validate it directly.

Dependency-light on purpose: stdlib ``socket`` + ``ipaddress`` only, so this
module is importable in any context without pulling in heavyweight deps.

Fail-closed: scheme problems, DNS-resolution failures, and unparseable hosts
all raise :class:`UnsafeURLError` rather than silently allowing the fetch.
"""
from __future__ import annotations

import ipaddress
import socket
from urllib.parse import urlsplit

from . import config

__all__ = ["UnsafeURLError", "assert_url_allowed", "assert_ip_allowed"]

_ALLOWED_SCHEMES = frozenset({"http", "https"})


class UnsafeURLError(ValueError):
    """Raised when a URL/IP is rejected by the SSRF guard."""


def _private_hosts_allowed() -> bool:
    # Read through the live module attribute so tests can monkeypatch
    # ``config.settings`` (or set the flag on it) and have it take effect.
    return bool(config.settings.allow_private_hosts)


def _ip_is_blocked(ip: ipaddress.IPv4Address | ipaddress.IPv6Address) -> bool:
    """True if ``ip`` falls in a range we must never fetch from.

    Covers loopback (127.0.0.0/8, ::1), link-local (169.254.0.0/16 incl. the
    169.254.169.254 metadata endpoint, fe80::/10), private/RFC1918 + ULA
    (fc00::/7), unspecified (0.0.0.0, ::), multicast, and anything else the
    stdlib flags as reserved.
    """
    # Normalise IPv4-mapped IPv6 (e.g. ::ffff:127.0.0.1) to the embedded v4
    # address so its loopback/private status is detected.
    mapped = getattr(ip, "ipv4_mapped", None)
    if mapped is not None:
        ip = mapped
    return (
        ip.is_private
        or ip.is_loopback
        or ip.is_link_local
        or ip.is_reserved
        or ip.is_multicast
        or ip.is_unspecified
    )


def _check_ip_str(ip: str) -> None:
    """Parse ``ip`` and raise if it is blocked (and private hosts disallowed)."""
    try:
        parsed = ipaddress.ip_address(ip)
    except ValueError as exc:  # pragma: no cover - getaddrinfo yields valid IPs
        raise UnsafeURLError(f"Could not parse IP address {ip!r}: {exc}") from exc
    if _ip_is_blocked(parsed) and not _private_hosts_allowed():
        raise UnsafeURLError(
            f"Refusing to connect to blocked address {ip} "
            "(loopback/link-local/private/reserved). "
            "Set allow_private_hosts=True to override."
        )


def assert_ip_allowed(ip: str) -> None:
    """Validate an already-resolved IP literal (for redirect-hop checks).

    Raises :class:`UnsafeURLError` when the IP is in a blocked range and
    ``settings.allow_private_hosts`` is not enabled.
    """
    _check_ip_str(ip)


def assert_url_allowed(url: str) -> str:
    """Validate ``url`` against the SSRF guard, returning it unchanged on success.

    Rejects non-http(s) schemes (file://, ftp://, gopher://, data:, ...) and any
    URL whose hostname resolves — via :func:`socket.getaddrinfo` — to *any*
    loopback/link-local/private/reserved address. Bare-IP literal hosts are
    checked directly. DNS-resolution failures raise :class:`UnsafeURLError`.
    """
    parts = urlsplit(url)
    scheme = parts.scheme.lower()
    if scheme not in _ALLOWED_SCHEMES:
        raise UnsafeURLError(
            f"Refusing URL with scheme {scheme or '(none)'!r}: "
            "only http and https are allowed."
        )

    host = parts.hostname
    if not host:
        raise UnsafeURLError(f"URL has no host to validate: {url!r}")

    # `parts.port` is a property that raises ValueError for an out-of-range or
    # non-numeric port. Convert it into the module's fail-closed UnsafeURLError
    # so read_doc / extract_structured surface a clean "URL refused" instead of
    # leaking a bare "Port out of range 0-65535".
    try:
        port = parts.port
    except ValueError as exc:
        raise UnsafeURLError(f"Invalid port in {url!r}: {exc}") from exc

    if _private_hosts_allowed():
        # Escape hatch fully engaged: skip the (network-touching) DNS resolution
        # entirely so private/local fetches work without leaking lookups.
        return url

    # If the host is a bare IP literal, check it directly without DNS.
    try:
        literal = ipaddress.ip_address(host)
    except ValueError:
        literal = None
    if literal is not None:
        _check_ip_str(str(literal))
        return url

    # Resolve to ALL A/AAAA addresses; block if ANY is unsafe.
    try:
        infos = socket.getaddrinfo(host, port, proto=socket.IPPROTO_TCP)
    except socket.gaierror as exc:
        raise UnsafeURLError(
            f"Could not resolve host {host!r}: {exc}. Refusing to connect."
        ) from exc

    addresses = {info[4][0] for info in infos}
    if not addresses:
        raise UnsafeURLError(
            f"Host {host!r} resolved to no addresses. Refusing to connect."
        )
    for addr in addresses:
        # getaddrinfo may append a scope id to link-local v6 (e.g. 'fe80::1%en0').
        _check_ip_str(addr.split("%", 1)[0])

    return url
