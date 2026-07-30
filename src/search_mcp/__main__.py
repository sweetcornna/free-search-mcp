import argparse

from .keystore import load_all_env_files
from .server import run


def _parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="search-mcp",
        description=(
            "Local-first, no-API-key search MCP server. Speaks MCP over stdio "
            "by default; pass --transport streamable-http to serve over HTTP."
        ),
    )
    parser.add_argument(
        "--transport",
        choices=["stdio", "streamable-http"],
        default=None,
        help="Transport to serve (default: stdio, or SEARCH_MCP_TRANSPORT).",
    )
    parser.add_argument(
        "--host",
        default=None,
        help=(
            "streamable-http bind address (default: 127.0.0.1). The server is "
            "unauthenticated and fetches arbitrary URLs — only bind a public "
            "address behind a proxy that terminates auth."
        ),
    )
    parser.add_argument(
        "--port", type=int, default=None,
        help="streamable-http port (default: 8000).",
    )
    parser.add_argument(
        "--path", default=None,
        help="streamable-http endpoint path (default: /mcp).",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> None:
    # Make SEARCH_MCP_* keys in ./.env AND <config_dir>/.env visible to the
    # keyed engines (keystore reads os.environ, which pydantic's .env loading
    # doesn't populate). The config-dir file covers uvx launches from any CWD.
    load_all_env_files()
    args = _parse_args(argv)
    # None means "not given on the command line" — run() then falls back to
    # the SEARCH_MCP_* setting, so CLI beats env beats default.
    run(
        transport=args.transport,
        host=args.host,
        port=args.port,
        path=args.path,
    )


if __name__ == "__main__":
    main()
