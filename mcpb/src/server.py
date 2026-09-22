"""Entry point of the Claude Desktop bundle.

The bundle holds no server code. `pyproject.toml` beside this directory pins
the published package, the host's `uv run` installs it, and this file starts
it — after one repair the host cannot be trusted to make.
"""

import os

# All three settings in manifest.json are optional, and a host fills in
# `${user_config.x}` whether or not the user typed anything. What an empty
# field becomes is up to the host: an empty string, or the placeholder left
# verbatim. Neither means "unset" to the server — SEARCH_MCP_CACHE_DIR=""
# parses as Path(""), so the cache would land in whatever directory the host
# happened to launch from, and "${user_config.proxy}" is not a proxy URL.
OPTIONAL_SETTINGS = ("SEARCH_MCP_PROXY", "SEARCH_MCP_REGION", "SEARCH_MCP_CACHE_DIR")


def drop_unfilled(environ=os.environ) -> None:
    for name in OPTIONAL_SETTINGS:
        value = environ.get(name)
        if value is not None and (not value.strip() or "${" in value):
            del environ[name]


if __name__ == "__main__":
    drop_unfilled()
    # Imported only now: the package reads its settings at import time.
    from search_mcp.__main__ import main

    main()
