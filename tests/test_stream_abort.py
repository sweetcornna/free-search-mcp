"""A streamed body nobody will read stops downloading.

The engines share long-lived curl sessions (sessions.py), and nothing can stop
a transfer on one of those once the caller has walked away from it. These
tests hold a local server to that: it sends 16 MB slowly, and each case checks
that the server saw the connection drop instead of finishing the body.
"""

from __future__ import annotations

import asyncio
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

import pytest

from search_mcp import sessions
from search_mcp.config import settings
from search_mcp.httpfetch import MaxBytesExceededError, curl_stream_capped
from search_mcp.sessions import AsyncSession

_CHUNK = 64 * 1024
_CHUNKS = 256  # 16 MB at 10 ms a chunk: about 2.6 s to send in full


@pytest.fixture(autouse=True)
def _fresh_pool():
    sessions.reset()
    yield
    sessions.reset()


@pytest.fixture
def big_server():
    """Serves /big slowly and records how its transfer ended. ``?delay=S``
    holds the headers back S seconds; ``?nolength`` sends no Content-Length."""
    state: dict = {"ended": None}

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_GET(self):  # noqa: N802
            query = parse_qs(urlsplit(self.path).query, keep_blank_values=True)
            try:
                time.sleep(float(query.get("delay", ["0"])[0]))
                self.send_response(200)
                if "nolength" in query:
                    self.send_header("Connection", "close")
                    self.close_connection = True
                else:
                    self.send_header("Content-Length", str(_CHUNK * _CHUNKS))
                self.end_headers()
                for _ in range(_CHUNKS):
                    self.wfile.write(b"x" * _CHUNK)
                    time.sleep(0.01)
            except OSError:
                state["ended"] = "aborted"
            else:
                state["ended"] = "complete"

        def log_message(self, *args):
            pass

    httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    thread.start()
    yield f"http://127.0.0.1:{httpd.server_address[1]}", state
    httpd.shutdown()
    httpd.server_close()


async def _how_it_ended(state: dict) -> str | None:
    for _ in range(100):
        if state["ended"]:
            return state["ended"]
        await asyncio.sleep(0.05)
    return None


async def test_a_stream_left_unread_stops_when_its_block_ends(big_server):
    base, state = big_server
    with pytest.raises(RuntimeError):
        async with AsyncSession(impersonate="chrome131", timeout=30) as client:
            await client.get(f"{base}/big", stream=True)
            raise RuntimeError("the status or the length was refused")
    assert await _how_it_ended(state) == "aborted"


async def test_a_stream_cancelled_before_its_headers_stops(big_server):
    base, state = big_server

    async def fetch():
        async with AsyncSession(impersonate="chrome131", timeout=30) as client:
            await client.get(f"{base}/big?delay=0.3", stream=True)

    with pytest.raises(TimeoutError):
        await asyncio.wait_for(fetch(), 0.1)
    assert await _how_it_ended(state) == "aborted"


async def test_a_body_over_the_cap_stops_downloading(big_server, monkeypatch):
    """No Content-Length, so the cap trips while streaming. Closing the stream
    used to wait for the rest of the body first."""
    base, state = big_server
    monkeypatch.setattr(settings, "max_response_bytes", 256 * 1024)
    start = time.monotonic()
    async with AsyncSession(impersonate="chrome131", timeout=30) as client:
        with pytest.raises(MaxBytesExceededError):
            await curl_stream_capped(client, f"{base}/big?nolength")
        assert time.monotonic() - start < 1.5
    assert await _how_it_ended(state) == "aborted"
