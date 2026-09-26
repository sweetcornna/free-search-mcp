"""Console entry point for the interactive sign-ins.

Two kinds of sign-in live here.

``search-mcp-login codex`` signs in with a ChatGPT account, the way the Codex
CLI does, so that the opt-in `codex` engine can run OpenAI's own web search on
the operator's plan. A browser opens, you approve, and the tokens are stored
under ``<config_dir>/oauth/``. When the browser runs on another machine, paste
the address it lands on into the terminal instead.

``search-mcp-login antigravity`` signs in with a Google account the way the
Antigravity IDE does, for the opt-in `antigravity` engine. Google's terms
forbid that use of the sign-in, and the command says so before it starts.

``search-mcp-login [zhihu|<url>]`` opens a real browser window so you can log in
to a site that the headless engines can't authenticate to with an API key (e.g.
zhihu). The persisted session is then reused by the browser pool.

Usage::

    search-mcp-login codex                   # sign in with ChatGPT (Codex)
    search-mcp-login codex --use-codex-cli   # reuse the Codex CLI's sign-in, read-only
    search-mcp-login antigravity             # sign in with Google (Antigravity)
    search-mcp-login status                  # who is signed in
    search-mcp-login logout codex            # forget a sign-in
    search-mcp-login                         # defaults to zhihu
    search-mcp-login zhihu
    search-mcp-login https://example.com/login
"""

from __future__ import annotations

import argparse
import asyncio
import sys
import time
from urllib.parse import parse_qs, urlparse

# Friendly aliases -> the URL the login flow opens.
_ALIASES: dict[str, str] = {"zhihu": "https://www.zhihu.com"}

_OAUTH_COMMANDS = ("codex", "antigravity", "status", "logout")


def _resolve(arg: str) -> str:
    """Map a CLI argument to a login URL (alias or a passed-through URL)."""
    return _ALIASES.get(arg, arg)


def _when(epoch: float) -> str:
    return time.strftime("%Y-%m-%d %H:%M", time.localtime(epoch)) if epoch else "unknown"


def _describe(info: dict) -> str:
    if not info.get("signed_in"):
        return f"not signed in (run `search-mcp-login {info['provider']}`)"
    who = info.get("email") or "an account with no email on record"
    bits = [f"signed in as {who}"]
    if info.get("plan"):
        bits.append(f"plan: {info['plan']}")
    if info.get("source") == "codex-cli":
        bits.append("linked to the Codex CLI sign-in (read-only)")
    if info.get("expired"):
        bits.append("EXPIRED")
    else:
        bits.append(f"access token valid until {_when(info.get('expires_at') or 0)}")
    return ", ".join(bits)


def _status() -> int:
    from . import oauth

    for pid in oauth.PROVIDERS:
        print(f"{pid:<12} {_describe(oauth.status(pid))}")
    return 0


def _logout(provider_id: str) -> int:
    from . import oauth

    if oauth.sign_out(provider_id):
        print(f"Signed out of {provider_id}: the stored tokens were deleted from this machine.")
    else:
        print(f"{provider_id} was not signed in.")
    return 0


def _sign_in(provider_id: str, *, open_browser: bool, codex_cli: str | None) -> int:
    from . import oauth

    spec = oauth.provider(provider_id)
    try:
        if codex_cli is not None:
            oauth.link_codex_cli(codex_cli or None)
        else:
            if spec.id == oauth.ANTIGRAVITY.id:
                print(f"Warning: {oauth.ANTIGRAVITY_WARNING}\n", flush=True)
                usage = "your account's Antigravity quota"
            else:
                usage = "your plan's Codex usage"
            print(f"Signing in to {spec.id} with your {spec.account} account. Searches "
                  f"will count against {usage}.")

            def show(url: str) -> None:
                redirect = parse_qs(urlparse(url).query).get("redirect_uri", [""])[0]
                print(
                    ("Opening your browser. If it does not open, visit this address:"
                     if open_browser else "Visit this address in a browser:")
                    + f"\n\n  {url}\n\n"
                    "If that browser is on another machine, sign in there, then copy the "
                    "address of the page it lands on (it starts with "
                    f"{redirect}?code=) and paste it here, followed by Enter.",
                    flush=True,
                )

            asyncio.run(
                oauth.login(provider_id, open_browser=open_browser, paste=True, on_url=show)
            )
    except (oauth.OAuthError, ValueError) as exc:
        print(f"Sign-in failed: {exc}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("\nSign-in cancelled.", file=sys.stderr)
        return 130
    print(f"\n{spec.id}: {_describe(oauth.status(provider_id))}")
    print(f'Search with it by naming it: search("...", engines=["{spec.engine}"])')
    return 0


def _oauth_main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        prog="search-mcp-login",
        description="Sign in so an opt-in engine (codex, antigravity) can search on your own "
        "account.",
    )
    sub = parser.add_subparsers(dest="command", required=True)
    cmd = sub.add_parser("codex", help="sign in with your ChatGPT account")
    cmd.add_argument(
        "--no-browser",
        action="store_true",
        help="print the sign-in address instead of opening a browser",
    )
    google = sub.add_parser(
        "antigravity",
        help="sign in with your Google account the way Antigravity does "
        "(against Google's terms; see the warning it prints)",
    )
    google.add_argument(
        "--no-browser",
        action="store_true",
        help="print the sign-in address instead of opening a browser",
    )
    cmd.add_argument(
        "--use-codex-cli",
        nargs="?",
        const="",
        default=None,
        metavar="AUTH_JSON",
        help="reuse the Codex CLI's sign-in ($CODEX_HOME/auth.json) read-only "
        "instead of signing in again",
    )
    sub.add_parser("status", help="show which accounts are signed in")
    out = sub.add_parser("logout", help="forget a stored sign-in")
    out.add_argument("provider", choices=["codex", "antigravity"])
    args = parser.parse_args(argv)

    if args.command == "status":
        return _status()
    if args.command == "logout":
        return _logout(args.provider)
    return _sign_in(
        args.command,
        open_browser=not args.no_browser,
        codex_cli=getattr(args, "use_codex_cli", None),
    )


def main() -> None:
    args = sys.argv[1:]
    if args and args[0] in _OAUTH_COMMANDS:
        # Settings and keys from ./.env and <config_dir>/.env, for the proxy.
        from .keystore import load_all_env_files

        load_all_env_files()
        sys.exit(_oauth_main(args))

    target = args[0] if args else "zhihu"
    url = _resolve(target)

    print("a browser window will open; log in, it auto-closes")

    # Import lazily so importing this module never pulls in Playwright.
    from .browser import pool

    ok = asyncio.run(pool.login(url))
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
