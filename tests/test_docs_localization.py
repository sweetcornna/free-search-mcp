from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def test_docs_describe_bilingual_admin_ui():
    files = [
        ROOT / "README.md",
        ROOT / "docs" / "API_KEYS.md",
        ROOT / "docs" / "USAGE.md",
    ]

    for path in files:
        text = path.read_text(encoding="utf-8")
        assert "中英双语" in text, path
        assert "uv run search-mcp-admin" in text, path
        assert "http://127.0.0.1:8765" in text, path
