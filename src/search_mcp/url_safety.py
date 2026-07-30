"""SSRF-guard helpers.

The MCP server fetches arbitrary user-supplied URLs. Without a guard, a caller
could point a tool at ``http://169.254.169.254/`` (cloud metadata), at
``http://127.0.0.1:.../`` (loopback services), or at any RFC1918 host reachable
from the server and exfiltrate internal data.

The guard is layered, and the layers differ in whether they need DNS:

1. **Scheme** — only http/https. No DNS.
2. **Address literals** — a host that IS an address (``127.0.0.1``,
   ``[::1]``, ``169.254.169.254``, ``100.100.100.200``) is checked directly.
   No DNS.
3. **Internal names** — ``localhost``, ``*.internal``, ``*.local``,
   ``metadata.google.internal`` and friends are refused by name. No DNS.
4. **Resolved addresses** — the hostname is resolved to *every* A/AAAA record
   and refused if any lands on a blocked range. Needs DNS, and is therefore
   only meaningful **when this process is the one connecting**.

When layer 4 applies
--------------------
Layer 4 only means something when THIS process's resolver decides what gets
connected to. Two common setups break that assumption, and running the layer
anyway did not merely add nothing — it refused every fetch and took the server
down:

  * **An outbound proxy.** The proxy resolves and connects; the address we
    would resolve is not the one used. On proxy-only egress the local resolver
    often cannot answer at all (fail-closed ⇒ "Could not resolve host …"), and
    on a censored network it answers with a blackhole address that the guard
    then read as an SSRF attempt.
  * **A fake-IP VPN** (Clash/Surge/sing-box in TUN mode). Every hostname is
    mapped into a tunnel range such as ``198.18.0.0/15``; the answer is a handle
    the tunnel translates, not a destination. Those ranges are reserved, so the
    guard classified every public host as private. Detected from canary
    hostnames that are public by definition, and only ever able to excuse the
    ranges in ``_FAKE_IP_CANDIDATES`` — never loopback, link-local or RFC1918.

In both cases layer 4 stands down and layers 1–3 — which need no DNS, and are
exactly the ones that stop a caller aiming the *proxy or tunnel* at loopback or
at a metadata endpoint — keep running. What is given up is catching a public
hostname whose DNS record points into a private network; on those setups the
egress path, not this process, is where that can be observed at all.

``ssrf_resolve_addresses`` ("auto" | "always" | "never") overrides the decision.

This matters because the failure mode it replaces is not "a fetch is refused"
but "the operator sets ``allow_private_hosts=true``" — trading a layer that
cannot work on their network for no guard at all, loopback and cloud metadata
included.

``assert_ip_allowed`` is provided for callers that have already resolved an IP
literal and want to validate it directly.

Dependency-light on purpose: stdlib ``socket`` + ``ipaddress`` plus the local
``net``/``config`` modules (themselves stdlib-only), so this stays importable
anywhere without pulling in heavyweight deps.

Fail-closed: scheme problems, unparseable hosts, and — on the unproxied path —
DNS-resolution failures all raise :class:`UnsafeURLError` rather than silently
allowing the fetch.
"""
from __future__ import annotations

import asyncio
import ipaddress
import socket
import time
from urllib.parse import urlsplit

from . import config, net

__all__ = [
    "UnsafeURLError",
    "assert_url_allowed",
    "assert_url_allowed_async",
    "assert_url_allowed_offline",
    "assert_ip_allowed",
    "clear_dns_cache",
]

_ALLOWED_SCHEMES = frozenset({"http", "https"})

# Ranges the stdlib flags do NOT already cover. Only CGNAT qualifies: 198.18/15,
# 192.0.0/24 and 64:ff9b::/96 all come back is_private/is_reserved from
# `ipaddress` and listing them here would imply a protection they do not add.
#
# 100.64.0.0/10 is carrier-grade NAT, reported as ordinary public space, yet it
# holds Alibaba Cloud's metadata endpoint at 100.100.100.200 — reachable from
# any instance there and every bit as sensitive as 169.254.169.254.
_EXTRA_BLOCKED_NETWORKS = (
    ipaddress.ip_network("100.64.0.0/10"),
)

# Hostnames that name an internal resource. Refused WITHOUT a DNS lookup, so
# they hold whether or not this process resolves the name — which is what makes
# them the layer that still protects a proxied request.
_INTERNAL_HOST_EXACT = frozenset({
    "localhost",
    "metadata",                    # k8s / docker short name
    "metadata.google.internal",    # GCP
    "metadata.goog",               # GCP
    "instance-data",               # AWS
    "instance-data.ec2.internal",  # AWS
})
_INTERNAL_HOST_SUFFIXES = (
    ".localhost",
    ".local",        # mDNS
    ".internal",     # ICANN-reserved for private use
    ".intranet",
    ".private",
    ".corp",
    ".lan",
    ".home.arpa",
)


def _host_is_internal(host: str) -> bool:
    """True when the hostname itself names an internal/link-local resource."""
    h = host.strip().rstrip(".").lower()
    if not h:
        return False
    return h in _INTERNAL_HOST_EXACT or h.endswith(_INTERNAL_HOST_SUFFIXES)


# Ranges a fake-IP tunnel may hand out as synthetic handles. Deliberately NOT
# the real private networks: a tunnel picks a range precisely because it never
# appears as a genuine destination, so auto-detection is allowed to stand down
# for these and NEVER for loopback, link-local, or RFC1918 — those stay blocked
# whatever the resolver claims.
_FAKE_IP_CANDIDATES = (
    ipaddress.ip_network("198.18.0.0/15"),   # IANA benchmarking; Clash/Surge default
    ipaddress.ip_network("28.0.0.0/8"),      # used by some fake-IP builds
    ipaddress.ip_network("fc00::/7"),        # v6 ULA
)

# Hostnames that are public by definition. If THESE resolve into a candidate
# range, the resolver is synthesising addresses for everything and layer 4
# cannot tell a public host from a private one.
_CANARY_HOSTS = ("example.com", "a.root-servers.net")

_synthetic_verdict: bool | None = None


def reset_resolver_detection() -> None:
    """Drop the memoised fake-IP verdict (tests re-stub the resolver)."""
    global _synthetic_verdict
    _synthetic_verdict = None


def _in_fake_ip_candidate(ip: ipaddress.IPv4Address | ipaddress.IPv6Address) -> bool:
    return any(ip in n for n in _FAKE_IP_CANDIDATES if ip.version == n.version)


def _resolver_is_synthetic() -> bool:
    """True when the local resolver maps every hostname into a tunnel range.

    A fake-IP VPN (Clash/Surge/sing-box in TUN mode) answers every lookup with a
    synthetic address that the tunnel maps back to the real destination. The
    address is a handle, not a destination, so checking it tells us nothing —
    and because those ranges are reserved, the guard read every public host as
    private and refused every fetch. That is what drove this machine to disable
    the guard wholesale, which is far worse than standing this one layer down.

    Established once per process from canary hostnames, and only ever able to
    excuse the ranges in ``_FAKE_IP_CANDIDATES``.
    """
    global _synthetic_verdict
    if _synthetic_verdict is not None:
        return _synthetic_verdict
    verdict = False
    for host in _CANARY_HOSTS:
        try:
            infos = socket.getaddrinfo(host, None, proto=socket.IPPROTO_TCP)
        except OSError:
            # No DNS at all says nothing about synthesis — stay strict.
            _synthetic_verdict = False
            return False
        addrs = {i[4][0].split("%", 1)[0] for i in infos}
        if not addrs:
            _synthetic_verdict = False
            return False
        try:
            parsed = [ipaddress.ip_address(a) for a in addrs]
        except ValueError:
            _synthetic_verdict = False
            return False
        if not all(_in_fake_ip_candidate(p) for p in parsed):
            # A canary landed on a real public address: the resolver is honest.
            _synthetic_verdict = False
            return False
        verdict = True
    _synthetic_verdict = verdict
    return verdict


def _request_is_proxied() -> bool:
    """Whether outbound fetches currently go through a proxy.

    The fetch paths (`httpfetch`, and through it `fetcher`/`documents`/
    `structured`) all use the UNSCOPED proxy, so the per-engine scope in
    ``net.proxy_for`` does not apply here.
    """
    try:
        return bool(net.proxy_url())
    except Exception:
        # A malformed keystore must not decide a security question by accident;
        # treat "unknown" as "not proxied" so the stricter path runs.
        return False

# A successful (host, port) validation is remembered briefly so a research()
# call that reads N pages from one site doesn't pay a DNS round trip for every
# page AND every redirect hop. Only SUCCESSES are cached — failures stay
# fail-closed and are re-checked on every call. The short TTL bounds how much
# this widens the already-documented rebinding TOCTOU window (the HTTP
# client's connect-time DNS was never pinned to the validated addresses).
_DNS_OK_TTL_SECONDS = 30.0
_DNS_OK_MAX_ENTRIES = 512
_dns_ok: dict[tuple[str, int | None], float] = {}


def clear_dns_cache() -> None:
    """Drop memoized validations (tests re-stub the resolver per test)."""
    _dns_ok.clear()


def _dns_ok_hit(host: str, port: int | None) -> bool:
    deadline = _dns_ok.get((host, port))
    if deadline is None:
        return False
    if time.monotonic() >= deadline:
        del _dns_ok[(host, port)]
        return False
    return True


def _dns_ok_store(host: str, port: int | None) -> None:
    if len(_dns_ok) >= _DNS_OK_MAX_ENTRIES:
        now = time.monotonic()
        for k in [k for k, dl in _dns_ok.items() if dl <= now]:
            del _dns_ok[k]
        if len(_dns_ok) >= _DNS_OK_MAX_ENTRIES:
            # Purely an optimization — dropping it entirely is always safe.
            _dns_ok.clear()
    _dns_ok[(host, port)] = time.monotonic() + _DNS_OK_TTL_SECONDS


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
    if (
        ip.is_private
        or ip.is_loopback
        or ip.is_link_local
        or ip.is_reserved
        or ip.is_multicast
        or ip.is_unspecified
    ):
        return True
    return any(ip in net_ for net_ in _EXTRA_BLOCKED_NETWORKS if ip.version == net_.version)


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


def _precheck(url: str) -> tuple[str, int | None] | None:
    """Shared scheme/host/port/literal validation for both guard variants.

    Returns ``(host, port)`` when a DNS resolution is still required, or
    ``None`` when the URL is already fully validated (private-hosts escape
    hatch engaged, or the host was an IP literal checked directly).
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
        return None

    # Layer 2 — if the host is a bare IP literal, check it directly without DNS.
    try:
        literal = ipaddress.ip_address(host)
    except ValueError:
        literal = None
    if literal is not None:
        _check_ip_str(str(literal))
        return None

    # Layer 3 — names that identify an internal resource. Deliberately BEFORE
    # the proxy check below: aiming a proxy at `metadata.google.internal` makes
    # the proxy read cloud credentials on the caller's behalf, so this is one of
    # the checks that matters MORE when proxied, not less.
    if _host_is_internal(host):
        raise UnsafeURLError(
            f"Refusing to connect to internal hostname {host!r} "
            "(localhost / .internal / .local / cloud metadata). "
            "Set allow_private_hosts=True to override."
        )

    # Layer 4 — resolve-and-check, but only when THIS process's resolver is what
    # decides the destination. Through a proxy the locally-resolved address is
    # not the one used, and insisting on it takes the server down wherever
    # direct DNS is unavailable or answers with a blackhole address.
    mode = config.settings.ssrf_resolve_addresses
    if mode == "never":
        return None
    if mode == "auto" and _request_is_proxied():
        return None

    return host, port


def _check_resolved(host: str, infos) -> None:
    """Validate every address ``getaddrinfo`` returned; block if ANY is unsafe."""
    addresses = {info[4][0] for info in infos}
    if not addresses:
        raise UnsafeURLError(
            f"Host {host!r} resolved to no addresses. Refusing to connect."
        )
    for addr in addresses:
        # getaddrinfo may append a scope id to link-local v6 (e.g. 'fe80::1%en0').
        clean = addr.split("%", 1)[0]
        try:
            parsed = ipaddress.ip_address(clean)
        except ValueError:
            _check_ip_str(clean)
            continue
        # A synthetic address from a fake-IP tunnel is a handle, not a
        # destination. Only consult the (network-touching) canary probe when we
        # are otherwise about to refuse, and only for the tunnel candidate
        # ranges — loopback/link-local/RFC1918 never reach this branch.
        if (
            _ip_is_blocked(parsed)
            and _in_fake_ip_candidate(parsed)
            and config.settings.ssrf_resolve_addresses == "auto"
            and _resolver_is_synthetic()
        ):
            continue
        _check_ip_str(clean)


def assert_url_allowed_offline(url: str) -> str:
    """Run only the layers that need no network: scheme, address literal and
    internal hostname. Returns the URL unchanged on success.

    For paths that answer WITHOUT connecting — a cache hit above all. Serving a
    cached body is still handing the caller the contents of that URL, so it has
    to obey the same policy; skipping the check meant anything fetched while the
    guard was permissive stayed retrievable forever afterwards, and tightening
    the setting did not take effect for it.

    Deliberately DNS-free so a cache hit stays a cache hit: the resolve-and-check
    layer only matters when we are about to open a connection.
    """
    _precheck(url)
    return url


def assert_url_allowed(url: str) -> str:
    """Validate ``url`` against the SSRF guard, returning it unchanged on success.

    Rejects non-http(s) schemes (file://, ftp://, gopher://, data:, ...) and any
    URL whose hostname resolves — via :func:`socket.getaddrinfo` — to *any*
    loopback/link-local/private/reserved address. Bare-IP literal hosts are
    checked directly. DNS-resolution failures raise :class:`UnsafeURLError`.

    This variant BLOCKS on DNS; async callers must use
    :func:`assert_url_allowed_async` so a slow lookup never stalls the event
    loop.
    """
    pending = _precheck(url)
    if pending is None:
        return url
    host, port = pending
    if _dns_ok_hit(host, port):
        return url
    try:
        infos = socket.getaddrinfo(host, port, proto=socket.IPPROTO_TCP)
    except socket.gaierror as exc:
        raise UnsafeURLError(
            f"Could not resolve host {host!r}: {exc}. Refusing to connect."
        ) from exc
    _check_resolved(host, infos)
    _dns_ok_store(host, port)
    return url


async def assert_url_allowed_async(url: str) -> str:
    """Same guard as :func:`assert_url_allowed`, resolving DNS through the
    event loop's executor (``loop.getaddrinfo``) so concurrent fetches keep
    running during a slow lookup."""
    pending = _precheck(url)
    if pending is None:
        return url
    host, port = pending
    if _dns_ok_hit(host, port):
        return url
    loop = asyncio.get_running_loop()
    try:
        infos = await loop.getaddrinfo(host, port, proto=socket.IPPROTO_TCP)
    except socket.gaierror as exc:
        raise UnsafeURLError(
            f"Could not resolve host {host!r}: {exc}. Refusing to connect."
        ) from exc
    _check_resolved(host, infos)
    _dns_ok_store(host, port)
    return url
