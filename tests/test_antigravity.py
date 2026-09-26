"""The opt-in `antigravity` engine: Google Search through Gemini on an Antigravity sign-in.

Offline. The Cloud Code backend and Google's grounding redirects are an
`httpx.MockTransport` that records what the engine sent, and the sign-in is a
credential written straight to the store.
"""
from __future__ import annotations

import json
import time
from datetime import datetime

import httpx
import pytest

from search_mcp import oauth
from search_mcp.config import settings
from search_mcp.engines import ENGINES, get_engine
from search_mcp.engines.antigravity import (
    AntigravityEngine,
    build_prompt,
    hits_from_reply,
    resolve_redirects,
)
from search_mcp.engines.base import EngineKeyError, SearchFilters
from search_mcp.engines.codex import Hit

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

REDIRECT = "https://vertexaisearch.cloud.google.com/grounding-api-redirect/"

# Where each grounding redirect leads. A key missing here answers 404.
PAGES = {
    "a": "https://docs.python.org/3/whatsnew/3.14.html",
    "b": "https://example.com/asyncio-314",
    "c": "https://example.org/uncited",
}


def _sign_in(**fields) -> None:
    oauth.save(oauth.Credential(**{
        "provider": "antigravity", "access_token": "ACCESS", "refresh_token": "r",
        "expires_at": time.time() + 3600, "account_id": "proj-9", "email": "me@gmail.com",
        "plan": "Google AI Pro", **fields,
    }))


def _redirect(request: httpx.Request) -> httpx.Response:
    target = PAGES.get(request.url.path.rsplit("/", 1)[-1])
    return httpx.Response(302, headers={"location": target}) if target else httpx.Response(404)


@pytest.fixture
def backend(monkeypatch):
    """`state["generate"]` answers generateContent; every request is kept."""
    calls: list[httpx.Request] = []
    state: dict = {}

    def factory(engine=None, *, timeout=30.0):
        def handle(request: httpx.Request) -> httpx.Response:
            calls.append(request)
            if request.url.host == "vertexaisearch.cloud.google.com":
                return _redirect(request)
            if request.url.path.endswith(":fetchAvailableModels"):
                listing = state["catalogue"]
                return listing(request) if callable(listing) else listing
            reply = state["generate"]
            return reply(request) if callable(reply) else reply

        return httpx.AsyncClient(transport=httpx.MockTransport(handle))

    monkeypatch.setattr(oauth, "http_client", factory)
    return state, calls


def _generated(calls: list[httpx.Request]) -> list[httpx.Request]:
    return [c for c in calls if c.url.path.endswith(":generateContent")]


def _reply(text: str, chunks: list[str], supports: list[tuple[str, list[int]]]) -> dict:
    """A generateContent reply. Each support names a piece of `text` and the
    chunks it rests on; its offsets are UTF-8 bytes, as the service counts."""
    raw = text.encode()
    grounding = []
    for piece, ids in supports:
        start = raw.index(piece.encode())
        segment = {"startIndex": start, "endIndex": start + len(piece.encode()), "text": piece}
        if start == 0:
            del segment["startIndex"]  # the service leaves a zero out
        grounding.append({"segment": segment, "groundingChunkIndices": ids})
    metadata = {
        "webSearchQueries": ["the query"],
        "groundingChunks": [{"web": {"uri": REDIRECT + key, "title": "site.example"}}
                            for key in chunks],
        "groundingSupports": grounding,
    }
    return {"response": {"candidates": [{
        "content": {"role": "model", "parts": [{"text": text}]},
        "finishReason": "STOP",
        "groundingMetadata": metadata,
    }]}, "traceId": "t"}


def _ungrounded(text: str = "Some page | undated | Written from memory.") -> dict:
    return {"response": {"candidates": [{
        "content": {"role": "model", "parts": [{"text": text}]}, "finishReason": "STOP",
    }]}}


LINE_1 = "What's new in Python 3.14 | 2025-10-07 | Adds asyncio introspection: python -m asyncio ps."
LINE_2 = ("asyncio changes explained | undated | A walk-through of the new CLI, see "
          "[the notes](https://made-up.example/notes).")
TEXT = f"{LINE_1}\n{LINE_2}\nA line nothing cites | undated | Nothing.\n"
GROUNDED = _reply(TEXT, ["a", "b", "c", "gone"],
                  [(LINE_1[:40], [0]), (LINE_2[:30], [1, 3])])


# --- wiring --------------------------------------------------------------------------


def test_it_is_registered_as_an_opt_in_web_engine():
    engine = get_engine("antigravity")
    assert isinstance(engine, AntigravityEngine)
    assert engine.categories == frozenset()
    assert engine.supports_browser_fallback is False
    assert "antigravity" not in settings.default_engines
    assert ENGINES["antigravity"].is_available() is False
    _sign_in()
    assert ENGINES["antigravity"].is_available() is True


async def test_without_a_sign_in_it_points_away_and_never_opens_a_sign_in(backend, monkeypatch):
    _, calls = backend
    # Even where `codex` would open its sign-in page by itself.
    monkeypatch.setattr(settings, "codex_auto_signin", True)
    monkeypatch.setattr(oauth, "can_open_browser", lambda: True)
    opened: list[str] = []
    monkeypatch.setattr(oauth, "open_in_browser", lambda url: opened.append(url) or True)

    with pytest.raises(EngineKeyError) as excinfo:
        await AntigravityEngine().search("anything", 5)

    message = str(excinfo.value)
    assert message.startswith("antigravity not configured")
    assert message.index("Nothing is wrong with the search") < message.index(
        "Do not ask the user for an API key or a sign-in"
    ) < message.index("search-mcp-login antigravity")
    assert calls == [] and opened == []


async def test_the_request_says_what_antigravity_says(backend):
    state, calls = backend
    state["generate"] = httpx.Response(200, json=GROUNDED)
    _sign_in()

    await AntigravityEngine().search("python 3.14 asyncio", 5,
                                     SearchFilters(freshness="week",
                                                   include_domains=["python.org"]))

    sent = _generated(calls)[0]
    assert str(sent.url) == (settings.antigravity_base_urls[0].rstrip("/")
                             + "/v1internal:generateContent")
    assert sent.headers["authorization"] == "Bearer ACCESS"
    # The backend licenses only what identifies as Antigravity.
    assert sent.headers["user-agent"].startswith(f"antigravity/{settings.antigravity_version} ")
    assert json.loads(sent.headers["client-metadata"])["ideType"] == "ANTIGRAVITY"
    body = json.loads(sent.content)
    assert body["project"] == "proj-9" and body["model"] == settings.antigravity_model
    request = body["request"]
    prompt = request["contents"][0]["parts"][0]["text"]
    assert "python 3.14 asyncio" in prompt and "Only pages from python.org." in prompt
    window = request["tools"][0]["googleSearch"]["timeRangeFilter"]
    start, end = (datetime.strptime(window[k], "%Y-%m-%dT%H:%M:%SZ") for k in ("startTime",
                                                                               "endTime"))
    assert (end - start).days == 7


def test_the_prompt_stays_short():
    # A longer prompt with a strict format made the model skip the search.
    prompt = build_prompt("q", 50, None)
    assert prompt.startswith("Search the web: q.") and "10 most relevant" in prompt
    assert len(prompt) < 250


# --- the reply ----------------------------------------------------------------------


async def test_grounded_lines_become_results_with_the_pages_real_addresses(backend):
    state, calls = backend
    state["generate"] = httpx.Response(200, json=GROUNDED)
    _sign_in()

    results = await AntigravityEngine().search("python 3.14 asyncio", 10)

    assert [(r.url, r.title) for r in results] == [
        ("https://docs.python.org/3/whatsnew/3.14.html", "What's new in Python 3.14"),
        ("https://example.com/asyncio-314", "asyncio changes explained"),
        # Returned by the search, cited by no line: after the cited ones.
        ("https://example.org/uncited", "example.org"),
    ]
    first = results[0]
    assert first.engine == "antigravity" and first.published_age == "2025-10-07"
    assert first.snippet.startswith("Adds asyncio introspection")
    # A redirect that does not resolve is dropped, and a URL the model wrote
    # itself never becomes a result.
    assert all("vertexaisearch" not in r.url and "made-up" not in r.url for r in results)
    # The redirects are followed without the sign-in.
    lookups = [c for c in calls if c.url.host == "vertexaisearch.cloud.google.com"]
    assert lookups and all("authorization" not in c.headers for c in lookups)


def test_support_offsets_are_bytes_not_characters():
    first = "杭州亚运会场馆 | 2024-01-02 | 场馆赛后向市民开放。"
    second = "第二个页面 | undated | 第二段内容。"
    data = _reply(f"{first}\n{second}\n", ["a", "b"], [(first, [0]), (second, [1])])
    # Only the offsets: a text match would hide a character-offset bug.
    for support in data["response"]["candidates"][0]["groundingMetadata"]["groundingSupports"]:
        support["segment"]["text"] = ""

    hits = hits_from_reply(data)

    assert [(h.url, h.title) for h in hits] == [(REDIRECT + "a", "杭州亚运会场馆"),
                                               (REDIRECT + "b", "第二个页面")]


def test_offsets_count_from_the_part_a_support_names():
    first = "First page | undated | About one thing.\n"
    second = "Second page | undated | About another.\n"
    data = _reply(first + second, ["a", "b"], [(first.strip(), [0])])
    candidate = data["response"]["candidates"][0]
    # The answer split in two text parts after a thought, as Gemini may send it.
    candidate["content"]["parts"] = [{"text": "", "thought": True},
                                     {"text": first}, {"text": second}]
    candidate["groundingMetadata"]["groundingSupports"].append({
        "segment": {"partIndex": 2, "startIndex": 0, "endIndex": len(second) - 1,
                    "text": ""},
        "groundingChunkIndices": [1],
    })

    hits = hits_from_reply(data)

    assert [(h.url, h.title) for h in hits] == [(REDIRECT + "a", "First page"),
                                               (REDIRECT + "b", "Second page")]


def test_a_short_segment_is_placed_by_its_offset_not_by_its_words():
    first = "Python 3.14 | undated | Python news."
    second = "Another page | undated | More Python."
    data = _reply(f"{first}\n{second}\n", ["a", "b"], [(first, [0]), (second, [1])])
    # A segment text that also appears on the other line.
    data["response"]["candidates"][0]["groundingMetadata"]["groundingSupports"][1][
        "segment"]["text"] = "Python"

    hits = hits_from_reply(data)

    assert [(h.url, h.title) for h in hits] == [(REDIRECT + "a", "Python 3.14"),
                                               (REDIRECT + "b", "Another page")]


async def test_only_redirects_up_to_the_limit_are_looked_up():
    looked_up: list[str] = []

    def handle(request: httpx.Request) -> httpx.Response:
        key = request.url.path.rsplit("/", 1)[-1]
        looked_up.append(key)
        return httpx.Response(302, headers={"location": f"https://site.example/{key}"})

    hits = [Hit(url=REDIRECT + str(i)) for i in range(5)] + [Hit(url=REDIRECT + "0"),
                                                            Hit(url="https://direct.example/")]
    async with httpx.AsyncClient(transport=httpx.MockTransport(handle)) as client:
        out = await resolve_redirects(client, hits, limit=3)

    assert sorted(looked_up) == ["0", "1", "2"]
    assert [h.url for h in out] == [
        "https://site.example/0", "https://site.example/1", "https://site.example/2",
        "https://site.example/0", "https://direct.example/",
    ]


async def test_an_unsearched_answer_is_asked_again_then_yields_nothing(backend):
    state, calls = backend
    state["generate"] = httpx.Response(200, json=_ungrounded())
    _sign_in()

    assert await AntigravityEngine().search("q", 5) == []
    assert len(_generated(calls)) == 2


async def test_a_second_ask_that_searches_is_used(backend):
    state, calls = backend
    replies = iter([httpx.Response(200, json=_ungrounded()), httpx.Response(200, json=GROUNDED)])
    state["generate"] = lambda request: next(replies)
    _sign_in()

    results = await AntigravityEngine().search("q", 5)

    assert len(_generated(calls)) == 2 and len(results) == 3


# --- failures ---------------------------------------------------------------------------


def _error(code: int, message: str, **extra) -> httpx.Response:
    return httpx.Response(code, json={"error": {"code": code, "message": message, **extra}})


async def test_a_host_out_of_quota_hands_over_to_the_next(backend):
    state, calls = backend
    replies = iter([_error(429, "Resource has been exhausted"), httpx.Response(200, json=GROUNDED)])
    state["generate"] = lambda request: next(replies)
    _sign_in()

    results = await AntigravityEngine().search("q", 5)

    hosts = [c.url.host for c in _generated(calls)]
    assert hosts == [httpx.URL(b).host for b in settings.antigravity_base_urls[:2]]
    assert results


async def test_quota_everywhere_says_when_it_resets(backend):
    state, _ = backend
    state["generate"] = _error(429, "Resource has been exhausted", status="RESOURCE_EXHAUSTED",
                               details=[{"@type": "type.googleapis.com/google.rpc.RetryInfo",
                                         "retryDelay": "42s"}])
    _sign_in()

    with pytest.raises(EngineKeyError, match=r"quota or rate limit.*resets in 42s"):
        await AntigravityEngine().search("q", 5)


async def test_a_refused_license_names_the_version_setting_and_stops(backend):
    state, calls = backend
    state["generate"] = _error(403, "You do not have a valid license of this product.")
    _sign_in()

    with pytest.raises(EngineKeyError, match="SEARCH_MCP_ANTIGRAVITY_VERSION"):
        await AntigravityEngine().search("q", 5)
    assert len(_generated(calls)) == 1


async def test_a_model_the_account_lacks_names_the_setting(backend):
    state, _ = backend
    state["generate"] = _error(400, "Requested model not found: gemini-0")
    _sign_in()

    with pytest.raises(EngineKeyError, match="SEARCH_MCP_ANTIGRAVITY_MODEL"):
        await AntigravityEngine().search("q", 5)


async def test_a_server_fault_everywhere_is_a_plain_error_the_breaker_counts(backend):
    state, calls = backend
    state["generate"] = _error(503, "The service is currently unavailable.")
    _sign_in()

    with pytest.raises(RuntimeError) as excinfo:
        await AntigravityEngine().search("q", 5)
    assert not isinstance(excinfo.value, EngineKeyError)
    assert len(_generated(calls)) == len(settings.antigravity_base_urls)


async def test_an_expired_token_is_refreshed_once_and_the_search_retried(backend, monkeypatch):
    state, calls = backend
    replies = iter([_error(401, "Request had invalid authentication credentials."),
                    httpx.Response(200, json=GROUNDED)])
    state["generate"] = lambda request: next(replies)
    forced: list[bool] = []

    async def credential(provider_id, *, force_refresh=False):
        forced.append(force_refresh)
        return oauth.Credential(provider="antigravity",
                                access_token="NEW" if force_refresh else "OLD",
                                account_id="proj-9", expires_at=time.time() + 3600)

    monkeypatch.setattr(oauth, "credential", credential)
    results = await AntigravityEngine().search("q", 5)

    assert forced == [False, True]
    assert [c.headers["authorization"] for c in _generated(calls)] == ["Bearer OLD", "Bearer NEW"]
    assert results


async def test_a_second_refusal_says_to_sign_in_again(backend, monkeypatch):
    state, _ = backend
    state["generate"] = _error(401, "Request had invalid authentication credentials.")

    async def credential(provider_id, *, force_refresh=False):
        return oauth.Credential(provider="antigravity", access_token="X",
                                expires_at=time.time() + 3600)

    monkeypatch.setattr(oauth, "credential", credential)
    with pytest.raises(EngineKeyError, match="search-mcp-login antigravity"):
        await AntigravityEngine().search("q", 5)


# --- the model: `latest` follows the catalogue --------------------------------------------

# The account's catalogue as the backend listed it on 2026-09-26 (trimmed).
CATALOGUE = {
    "models": {
        "gemini-3.8-flash-tiered": {"recommended": True},
        "gemini-3.6-flash-low": {"recommended": True, "displayName": "Gemini 3.6 Flash (Low)"},
        "gemini-3.5-flash-lite": {"recommended": True},
        "gemini-3.1-flash-lite": {"displayName": "Gemini 3.1 Flash Lite"},
        "gemini-3.1-flash-image": {"recommended": True},
        "gemini-3-flash": {"recommended": True},
        "claude-sonnet-4-6": {"recommended": True},
    },
    "tieredModelIds": {"flashLite": ["gemini-3.5-flash-lite"],
                       "flash": ["gemini-3.8-flash-tiered"], "pro": ["gemini-3.1-pro-low"]},
    "webSearchModelIds": ["gemini-3.1-flash-lite"],
    "defaultAgentModelId": "gemini-3.6-flash-high",
}


def test_latest_is_the_catalogues_flash_model_then_its_search_model():
    from search_mcp.engines.antigravity import pick_models

    assert pick_models(CATALOGUE) == ("gemini-3.8-flash-tiered", "gemini-3.1-flash-lite")
    # Google ships a new flash model: it is used as soon as the catalogue names it.
    newer = {**CATALOGUE, "tieredModelIds": {"flash": ["gemini-4-flash"]}}
    assert pick_models(newer)[0] == "gemini-4-flash"
    # Without the pointer, the newest recommended flash that is not lite or image.
    bare = {"models": CATALOGUE["models"]}
    assert pick_models(bare) == ("gemini-3.8-flash-tiered", "gemini-3.8-flash-tiered")
    # Nor a `-low` one, the kind that never searched.
    same = {"models": {"gemini-3.9-flash-high": {"recommended": True},
                       "gemini-3.9-flash-low": {"recommended": True}}}
    assert pick_models(same)[0] == "gemini-3.9-flash-high"
    assert pick_models({}) == pick_models(None) == ("", "")


@pytest.fixture
def latest(monkeypatch):
    from search_mcp.engines import antigravity as antigravity_module

    monkeypatch.setattr(settings, "antigravity_model", "latest")
    monkeypatch.setattr(antigravity_module, "_latest", {})


async def test_an_unsearched_answer_is_asked_again_of_the_search_model(backend, latest):
    state, calls = backend
    state["catalogue"] = httpx.Response(200, json=CATALOGUE)
    replies = iter([httpx.Response(200, json=_ungrounded()), httpx.Response(200, json=GROUNDED),
                    httpx.Response(200, json=GROUNDED)])
    state["generate"] = lambda request: next(replies)
    _sign_in()

    assert await AntigravityEngine().search("q1", 5)
    assert await AntigravityEngine().search("q2", 5)

    models = [json.loads(c.content)["model"] for c in _generated(calls)]
    assert models == ["gemini-3.8-flash-tiered", "gemini-3.1-flash-lite",
                      "gemini-3.8-flash-tiered"]
    listing = [c for c in calls if c.url.path.endswith(":fetchAvailableModels")]
    assert len(listing) == 1 and json.loads(listing[0].content) == {"project": "proj-9"}


async def test_an_unreadable_catalogue_falls_back_and_the_search_still_runs(backend, latest):
    from search_mcp.engines.antigravity import _FALLBACK_MODEL

    state, calls = backend
    state["catalogue"] = _error(503, "unavailable")
    state["generate"] = httpx.Response(200, json=GROUNDED)
    _sign_in()

    assert await AntigravityEngine().search("q", 5)
    assert json.loads(_generated(calls)[0].content)["model"] == _FALLBACK_MODEL


def _listings(calls: list[httpx.Request]) -> list[httpx.Request]:
    return [c for c in calls if c.url.path.endswith(":fetchAvailableModels")]


async def test_a_refused_pick_hands_over_to_the_other_and_is_looked_up_again(backend, latest):
    state, calls = backend
    state["catalogue"] = httpx.Response(200, json=CATALOGUE)
    replies = iter([_error(400, "Model gemini-3.8-flash-tiered is not found for this project."),
                    httpx.Response(200, json=GROUNDED), httpx.Response(200, json=GROUNDED)])
    state["generate"] = lambda request: next(replies)
    _sign_in()

    assert await AntigravityEngine().search("q1", 5)
    assert await AntigravityEngine().search("q2", 5)

    models = [json.loads(c.content)["model"] for c in _generated(calls)]
    assert models == ["gemini-3.8-flash-tiered", "gemini-3.1-flash-lite",
                      "gemini-3.8-flash-tiered"]
    assert len(_listings(calls)) == 2


async def test_a_refused_pinned_model_is_reported_once(backend):
    state, calls = backend
    state["generate"] = _error(404, "Requested entity was not found: model gemini-x.")
    _sign_in()

    with pytest.raises(EngineKeyError, match="or to latest"):
        await AntigravityEngine().search("q", 5)
    assert len(_generated(calls)) == 1


async def test_each_project_has_its_own_catalogue_answer(backend, latest):
    from search_mcp.engines import antigravity as antigravity_module

    state, calls = backend
    state["catalogue"] = _error(503, "unavailable")
    state["generate"] = httpx.Response(200, json=GROUNDED)
    _sign_in()

    await AntigravityEngine().search("q1", 5)
    # A failed lookup is kept for five minutes, a good one for six hours.
    expires, _ = antigravity_module._latest["proj-9"]
    assert 290 < expires - time.monotonic() <= 300
    state["catalogue"] = httpx.Response(200, json=CATALOGUE)
    _sign_in(account_id="proj-10")
    await AntigravityEngine().search("q2", 5)
    expires, _ = antigravity_module._latest["proj-10"]
    assert 6 * 3600 - 10 < expires - time.monotonic() <= 6 * 3600

    assert [json.loads(c.content) for c in _listings(calls)][-1] == {"project": "proj-10"}


async def test_a_refused_token_on_the_catalogue_is_refreshed(backend, latest, monkeypatch):
    state, calls = backend
    listings = iter([_error(401, "Request had invalid authentication credentials."),
                     httpx.Response(200, json=CATALOGUE)])
    state["catalogue"] = lambda request: next(listings)
    state["generate"] = httpx.Response(200, json=GROUNDED)
    forced: list[bool] = []

    async def credential(provider_id, *, force_refresh=False):
        forced.append(force_refresh)
        return oauth.Credential(provider="antigravity",
                                access_token="NEW" if force_refresh else "OLD",
                                account_id="proj-9", expires_at=time.time() + 3600)

    monkeypatch.setattr(oauth, "credential", credential)
    assert await AntigravityEngine().search("q", 5)

    assert forced == [False, True]
    assert json.loads(_generated(calls)[0].content)["model"] == "gemini-3.8-flash-tiered"
