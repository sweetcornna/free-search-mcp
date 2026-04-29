"""Unit tests for `extract_structured`.

We feed extruct hand-crafted HTML rather than hitting the network — the
goal is to verify our wiring around extruct, not extruct itself.
"""
from __future__ import annotations

import pytest

_JSONLD_HTML = """\
<!DOCTYPE html>
<html>
<head>
  <title>Test article</title>
  <meta property="og:title" content="OG Title">
  <meta property="og:type" content="article">
  <meta property="og:url" content="https://example.com/article">
  <meta name="twitter:card" content="summary_large_image">
  <meta name="twitter:title" content="Twitter Title">
  <script type="application/ld+json">
  {
    "@context": "https://schema.org",
    "@type": "Article",
    "headline": "X",
    "author": {"@type": "Person", "name": "Jane Doe"},
    "datePublished": "2024-01-15"
  }
  </script>
</head>
<body>
  <article itemscope itemtype="https://schema.org/Article">
    <h1 itemprop="headline">Microdata headline</h1>
    <span itemprop="author">Bob</span>
  </article>
</body>
</html>
"""


def test_extract_structured_from_html_parses_jsonld():
    from search_mcp.structured import extract_structured_from_html

    payload = extract_structured_from_html(_JSONLD_HTML, "https://example.com/article")
    assert payload["url"] == "https://example.com/article"
    assert isinstance(payload["json_ld"], list)
    assert payload["json_ld"], "JSON-LD block should be parsed out"
    article = payload["json_ld"][0]
    # uniform=True normalises @type at the top level.
    assert article.get("@type") == "Article"
    assert article.get("headline") == "X"


def test_extract_structured_from_html_parses_opengraph():
    from search_mcp.structured import extract_structured_from_html

    payload = extract_structured_from_html(_JSONLD_HTML, "https://example.com/article")
    og = payload["opengraph"]
    assert og, "OpenGraph block should be parsed out"
    # extruct may shape this as a list of dicts; just assert "OG Title" appears
    # somewhere in the serialised payload.
    import json
    blob = json.dumps(og)
    assert "OG Title" in blob
    assert "article" in blob


def test_extract_structured_from_html_parses_microdata():
    from search_mcp.structured import extract_structured_from_html

    payload = extract_structured_from_html(_JSONLD_HTML, "https://example.com/article")
    md = payload["microdata"]
    assert md, "microdata block should be parsed out"
    import json
    blob = json.dumps(md)
    assert "Microdata headline" in blob


def test_extract_structured_from_html_empty_page():
    from search_mcp.structured import extract_structured_from_html

    payload = extract_structured_from_html(
        "<html><body><p>nothing</p></body></html>",
        "https://example.com/blank",
    )
    assert payload["url"] == "https://example.com/blank"
    assert payload["json_ld"] == []
    assert payload["microdata"] == []
    # opengraph + rdfa may also be empty, but that's not required


def test_render_structured_markdown_contains_sections():
    from search_mcp.formatting import render_structured

    payload = {
        "url": "https://example.com/article",
        "json_ld": [{"@type": "Article", "headline": "X"}],
        "microdata": [],
        "opengraph": [{"og:title": "OG"}],
        "rdfa": [],
    }
    md = render_structured(payload)
    assert "# Structured data: https://example.com/article" in md
    assert "## json_ld" in md
    assert "## opengraph" in md
    assert "## microdata" not in md  # empty list -> skip
    assert '"@type": "Article"' in md
    assert "OG" in md


def test_render_structured_markdown_when_nothing_found():
    from search_mcp.formatting import render_structured

    payload = {
        "url": "https://example.com/blank",
        "json_ld": [],
        "microdata": [],
        "opengraph": [],
        "rdfa": [],
    }
    md = render_structured(payload)
    assert "No structured data found" in md


@pytest.mark.asyncio
async def test_extract_structured_async_uses_httpx(monkeypatch):
    """The async wrapper should fetch the URL itself (not via the page cache)."""
    from search_mcp import structured as structured_mod

    captured: dict[str, str] = {}

    class _Resp:
        text = _JSONLD_HTML

        def raise_for_status(self):
            return None

    class _FakeClient:
        def __init__(self, *a, **kw):
            pass

        async def __aenter__(self):
            return self

        async def __aexit__(self, *a):
            return None

        async def get(self, url):
            captured["url"] = url
            return _Resp()

    monkeypatch.setattr(structured_mod.httpx, "AsyncClient", _FakeClient)

    payload = await structured_mod.extract_structured("https://example.com/article")
    assert captured["url"] == "https://example.com/article"
    assert payload["url"] == "https://example.com/article"
    assert payload["json_ld"], "json-ld parsed via async path"
