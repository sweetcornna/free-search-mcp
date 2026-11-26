# syntax=docker/dockerfile:1
# Containerized free-search-mcp 1.0 (distroless Rust binary, ~25 MB).
#
# Defaults to MCP over stdio, so run attached:
#   docker run -i --rm free-search-mcp
# (or use docker-compose.yml, which sets stdin_open: true).
#
# Streamable HTTP mode (configure SEARCH_MCP_HTTP_TOKEN before exposing beyond loopback):
#   docker run --rm -p 8000:8000 \
#     -e SEARCH_MCP_HTTP_TOKEN="change-me" \
#     free-search-mcp --transport streamable-http --host 0.0.0.0 --port 8000

FROM rust:1.98-slim-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY xtask ./xtask

RUN cargo build --release --locked -p fsm-server --bin free-search-mcp \
    && mkdir -p /out/usr/local/bin /out/data /out/config \
    && cp target/release/free-search-mcp /out/usr/local/bin/free-search-mcp \
    && ln -s free-search-mcp /out/usr/local/bin/search-mcp \
    && chown -R 65532:65532 /out/data /out/config

FROM gcr.io/distroless/cc-debian12:nonroot

ENV SEARCH_MCP_CACHE_DIR=/data \
    SEARCH_MCP_CONFIG_DIR=/config

COPY --from=builder /out/usr/local/bin/free-search-mcp /usr/local/bin/free-search-mcp
COPY --from=builder /out/usr/local/bin/search-mcp /usr/local/bin/search-mcp
COPY --from=builder --chown=65532:65532 /out/data /data
COPY --from=builder --chown=65532:65532 /out/config /config

VOLUME ["/data", "/config"]
USER nonroot:nonroot

# stdio transport by default — keep STDIN open when running this image.
ENTRYPOINT ["/usr/local/bin/free-search-mcp"]
