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


def test_extract_structured_meta_fallback_when_no_structured_data():
    """Page with only bare <meta> tags: meta_fallback populated, hint mentions fallback."""
    from search_mcp.structured import extract_structured_from_html

    html = """\
<!DOCTYPE html>
<html>
<head>
  <title>Plain page</title>
  <meta name="description" content="A plain page about cats.">
  <meta name="keywords" content="cats, felines">
  <meta name="author" content="A. Person">
  <meta name="twitter:title" content="Cats!">
</head>
<body><p>hi</p></body>
</html>
"""
    payload = extract_structured_from_html(html, "https://example.com/plain")
    # All five structured syntaxes empty
    assert payload["json_ld"] == []
    assert payload["microdata"] == []
    assert payload["opengraph"] == []
    assert payload["rdfa"] == []
    assert payload["microformat"] == []
    # Fallback fired
    assert payload.get("meta_fallback"), "meta_fallback should be populated"
    assert payload["meta_fallback"].get("description") == "A plain page about cats."
    assert payload["meta_fallback"].get("keywords") == "cats, felines"
    assert payload["meta_fallback"].get("author") == "A. Person"
    assert payload["meta_fallback"].get("twitter:title") == "Cats!"
    # Hint present and references the meta_fallback path
    assert "hint" in payload
    assert "meta_fallback" in payload["hint"]
    # Strong "no fallback either" suffix should NOT be present here.
    assert "bot-block shell" not in payload["hint"]


def test_extract_structured_completely_empty_html_strong_hint():
    """Empty <html></html>: meta_fallback empty, hint includes strongest suffix."""
    from search_mcp.structured import extract_structured_from_html

    payload = extract_structured_from_html("<html></html>", "https://example.com/empty")
    assert payload["json_ld"] == []
    assert payload["microdata"] == []
    assert payload["opengraph"] == []
    assert payload["rdfa"] == []
    assert payload["microformat"] == []
    assert payload.get("meta_fallback") == {}
    assert "hint" in payload
    assert "bot-block shell" in payload["hint"]


def test_extract_structured_jsonld_plus_meta_no_hint():
    """When real structured data is present, no hint and no meta_fallback fires."""
    from search_mcp.structured import extract_structured_from_html

    html = """\
<!DOCTYPE html>
<html>
<head>
  <meta name="description" content="A described page.">
  <script type="application/ld+json">
  {"@context": "https://schema.org", "@type": "Article", "headline": "Y"}
  </script>
</head>
<body></body>
</html>
"""
    payload = extract_structured_from_html(html, "https://example.com/rich")
    assert payload["json_ld"], "json_ld should be parsed"
    assert "hint" not in payload
    assert "meta_fallback" not in payload


def test_extract_structured_microformats2_h_card():
    """h-card microformat should be parsed into the `microformat` list."""
    from search_mcp.structured import extract_structured_from_html

    # Canonical h-card example from the microformats2 spec.
    html = """\
<!DOCTYPE html>
<html>
<body>
  <div class="h-card">
    <a class="p-name u-url" href="https://janedoe.example.com/">Jane Doe</a>
    <span class="p-job-title">Engineer</span>
  </div>
</body>
</html>
"""
    payload = extract_structured_from_html(html, "https://janedoe.example.com/")
    assert isinstance(payload.get("microformat"), list)
    assert payload["microformat"], "h-card should be parsed into microformat list"
    import json
    blob = json.dumps(payload["microformat"])
    assert "Jane Doe" in blob
    # Real structured data found, so no hint.
    assert "hint" not in payload


def test_render_structured_shows_hint_and_meta_fallback():
    """render_structured should show the hint at top and meta tags as a table."""
    from search_mcp.formatting import render_structured

    payload = {
        "url": "https://example.com/plain",
        "json_ld": [],
        "microdata": [],
        "opengraph": [],
        "rdfa": [],
        "microformat": [],
        "meta_fallback": {"description": "Hi there", "author": "X"},
        "hint": "No JSON-LD ... bare meta tags surfaced as `meta_fallback` if any.",
    }
    md = render_structured(payload)
    assert "No structured data found" in md
    assert "meta_fallback" in md  # hint surfaces the field name
    assert "## Meta tags" in md
    assert "`description`" in md
    assert "Hi there" in md
    assert "`author`" in md


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
