"""The shared curl sessions: one connection per identity, no cookies carried over."""

import asyncio
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from types import SimpleNamespace

import pytest
from curl_cffi.requests import Headers

from search_mcp import sessions
from search_mcp.sessions import AsyncSession


@pytest.fixture(autouse=True)
def _fresh_pool():
    sessions.reset()
    yield
    sessions.reset()


@pytest.fixture
def server():
    """A keep-alive HTTP/1.1 server that sets a cookie on /set, echoes the
    Cookie header on /echo, and records which client port each request came
    from (one port per TCP connection)."""
    ports: list[int] = []

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_GET(self):  # noqa: N802
            ports.append(self.client_address[1])
            body = (self.headers.get("Cookie") or "").encode()
            self.send_response(200)
            if self.path == "/set":
                self.send_header("Set-Cookie", "tracker=1; Path=/")
            elif self.path == "/unset":
                self.send_header("Set-Cookie", "tracker=; Max-Age=0; Path=/")
            elif self.path == "/elsewhere":
                self.send_header("Set-Cookie", "tracker=1; Domain=victim.test; Path=/")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    thread.start()
    yield f"http://127.0.0.1:{httpd.server_address[1]}", ports
    httpd.shutdown()
    httpd.server_close()


async def test_blocks_with_one_identity_share_a_session():
    async with AsyncSession(impersonate="chrome131", timeout=5) as a:
        first = a._client()
    async with AsyncSession(impersonate="chrome131", timeout=9, headers={"X": "1"}) as b:
        assert b._client() is first
    async with AsyncSession(impersonate="edge101") as c:
        assert c._client() is not first
    async with AsyncSession(impersonate="chrome131", proxy="http://127.0.0.1:9") as d:
        assert d._client() is not first


async def test_constructor_kwargs_become_request_defaults(monkeypatch):
    seen = {}

    async def fake_request(self, method, url, **kwargs):
        seen.update(kwargs, method=method, url=url)
        return SimpleNamespace(url=url, headers=Headers())

    monkeypatch.setattr(sessions._CurlSession, "request", fake_request)
    async with AsyncSession(
        impersonate="chrome131",
        timeout=7,
        allow_redirects=False,
        headers={"Accept": "a", "Accept-Language": "en"},
    ) as client:
        await client.get("https://example.com/", headers={"Accept": "b"}, stream=True)
    assert seen["method"] == "GET"
    assert seen["timeout"] == 7
    assert seen["allow_redirects"] is False
    assert seen["stream"] is True
    assert seen["headers"] == {"Accept": "b", "Accept-Language": "en"}
    assert "impersonate" not in seen


async def test_requests_reuse_one_connection(server):
    base, ports = server
    for _ in range(3):
        async with AsyncSession(impersonate="chrome131", timeout=5) as client:
            resp = await client.get(f"{base}/echo")
            assert resp.status_code == 200
    assert len(ports) == 3
    assert len(set(ports)) == 1, f"expected one connection, saw ports {ports}"


async def test_cookies_last_as_long_as_the_block(server):
    """Within a block a cookie set by one response goes out with the next
    request (a manual redirect relies on it); the next block starts empty."""
    base, _ = server
    async with AsyncSession(impersonate="chrome131", timeout=5) as client:
        await client.get(f"{base}/set")
        same_block = await client.get(f"{base}/echo")
    async with AsyncSession(impersonate="chrome131", timeout=5) as client:
        next_block = await client.get(f"{base}/echo")
    async with AsyncSession(impersonate="chrome131", timeout=5, cookies={"seed": "1"}) as client:
        seeded = await client.get(f"{base}/echo", cookies={"mine": "2"})
    assert same_block.text == "tracker=1"
    assert next_block.text == ""
    assert sorted(seeded.text.split("; ")) == ["mine=2", "seed=1"]


async def test_a_host_only_cookie_stays_with_its_host(server):
    base, _ = server
    other = base.replace("127.0.0.1", "localhost")
    async with AsyncSession(impersonate="chrome131", timeout=5) as client:
        await client.get(f"{base}/set")
        elsewhere = await client.get(f"{other}/echo")
    assert elsewhere.text == ""


async def test_a_response_cannot_set_a_cookie_for_another_site(server):
    base, _ = server
    async with AsyncSession(impersonate="chrome131", timeout=5) as client:
        await client.get(f"{base}/elsewhere")
        assert list(client.cookies.jar) == []


async def test_max_age_zero_deletes_the_cookie(server):
    base, _ = server
    async with AsyncSession(impersonate="chrome131", timeout=5) as client:
        await client.get(f"{base}/set")
        await client.get(f"{base}/unset")
        after = await client.get(f"{base}/echo")
    assert after.text == ""


def test_a_new_event_loop_gets_new_sessions():
    async def grab():
        async with AsyncSession(impersonate="chrome131") as client:
            return client._client()

    first = asyncio.run(grab())
    second = asyncio.run(grab())
    assert first is not second


async def test_use_outside_the_block_is_an_error():
    client = AsyncSession(impersonate="chrome131")
    with pytest.raises(RuntimeError):
        await client.get("https://example.com/")
