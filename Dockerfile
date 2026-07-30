# Containerized free-search-mcp. Defaults to MCP over stdio, so run it
# attached:   docker run -i --rm search-mcp
# (or use docker-compose, which sets stdin_open).
#
# To serve over HTTP instead — protocol revision 2026-07-28 is stateless, so
# no sticky sessions are needed and replicas scale horizontally:
#   docker run --rm -p 8000:8000 search-mcp \
#     uv run search-mcp --transport streamable-http --host 0.0.0.0
# That endpoint is UNAUTHENTICATED and fetches arbitrary URLs on the caller's
# behalf. Only publish the port behind a proxy that terminates auth.
FROM python:3.12-slim

ENV PYTHONUNBUFFERED=1 \
    UV_LINK_MODE=copy \
    UV_COMPILE_BYTECODE=1 \
    SEARCH_MCP_CACHE_DIR=/data

# uv (fast Python package manager), pinned-free latest.
COPY --from=ghcr.io/astral-sh/uv:latest /uv /uvx /bin/

WORKDIR /app

# Install deps first (better layer caching), then the source.
COPY pyproject.toml uv.lock README.md ./
COPY src ./src
RUN uv sync --frozen --no-dev

# Chromium + its OS deps, matched to the installed Playwright (browser-rendered
# engines: startpage/bing/zhihu, and the fetch browser fallback).
RUN uv run playwright install --with-deps chromium

VOLUME /data
# stdio transport — keep STDIN open when running this image.
CMD ["uv", "run", "search-mcp"]
