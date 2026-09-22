import argparse
import json
import sys

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
            "unauthenticated and fetches arbitrary URLs, so only bind a public "
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


def _ask(argv: list[str]) -> int:
    """`search-mcp ask "question"`: the answer agent as a one-shot command, for
    a service that would rather start a process than speak MCP. Exit status 0
    means an answer, 1 means the model failed and the pages were printed
    instead, 2 means the agent is not configured."""
    import asyncio

    from .agent import AgentError, ask
    from .formatting import render_ask

    parser = argparse.ArgumentParser(
        prog="search-mcp ask",
        description=(
            "Search, read the top pages, and have the configured model answer. "
            "Needs SEARCH_MCP_AGENT_BACKEND (api, claude-code or codex)."
        ),
    )
    parser.add_argument("question")
    parser.add_argument("--freshness", choices=["day", "week", "month", "year"])
    parser.add_argument(
        "--domain", action="append", dest="domains", metavar="DOMAIN",
        help="Restrict the search to this domain. Repeatable.",
    )
    parser.add_argument("--json", action="store_true", help="Print the JSON form.")
    args = parser.parse_args(argv)
    try:
        payload = asyncio.run(
            ask(args.question, freshness=args.freshness, include_domains=args.domains)
        )
    except (AgentError, ValueError) as exc:
        print(f"search-mcp ask: {exc}", file=sys.stderr)
        return 2
    if args.json:
        print(json.dumps(payload, ensure_ascii=False, indent=2, default=str))
    else:
        print(render_ask(payload), end="")
    return 0 if payload.get("answer") else 1


def _agent_file(argv: list[str]) -> int:
    """`search-mcp agent-file codex > ~/.codex/agents/quick_search.toml`: the
    quick-search agent in a host's own format, for installs that do not come
    through the Claude Code plugin."""
    from .agent import STANDALONE_TOOL_PREFIX, agent_file

    parser = argparse.ArgumentParser(
        prog="search-mcp agent-file",
        description="Print the quick-search agent definition for a host.",
    )
    parser.add_argument(
        "host", choices=["claude-code", "codex", "prompt"],
        help="claude-code: a .claude/agents/*.md file. codex: a .codex/agents/*.toml file. "
        "prompt: the bare instructions, for any other platform.",
    )
    parser.add_argument(
        "--tool-prefix", default=STANDALONE_TOOL_PREFIX,
        help="claude-code only: how the host names this server's tools "
        f"(default: {STANDALONE_TOOL_PREFIX}, which matches `claude mcp add search ...`).",
    )
    args = parser.parse_args(argv)
    print(agent_file(args.host, tool_prefix=args.tool_prefix), end="")
    return 0


def main(argv: list[str] | None = None) -> None:
    # Make SEARCH_MCP_* keys in ./.env AND <config_dir>/.env visible to the
    # keyed engines (keystore reads os.environ, which pydantic's .env loading
    # doesn't populate). The config-dir file covers uvx launches from any CWD.
    load_all_env_files()
    argv = sys.argv[1:] if argv is None else argv
    if argv[:1] == ["ask"]:
        raise SystemExit(_ask(argv[1:]))
    if argv[:1] == ["agent-file"]:
        raise SystemExit(_agent_file(argv[1:]))
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
