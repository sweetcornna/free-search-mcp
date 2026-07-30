"""GitHub search — repositories and issues/PRs.

Two engines with different key requirements, which is why they live together:

  * `github`      — repositories + issues. Works keyless (10 req/min per IP);
                    a configured token raises that to 30 req/min.
  * `github_code` — code *content* search. GitHub returns 401 to anonymous
                    callers, so this one is keyed like brave_api/serper and
                    raises an actionable error when no token is configured.

The keyless engine follows the never-raise rule; the keyed one raises so the
aggregator can tell the user their token is missing or rejected, rather than
reporting a silent empty result.
"""

from __future__ import annotations

from typing import Any
from urllib.parse import quote_plus

from ..keystore import get_secret
from .base import EngineKeyError, SearchFilters, SearchResult, raise_for_key_error
from .jsonapi import JsonApiEngine, clip, iso_date

_API = "https://api.github.com"
_ACCEPT = "application/vnd.github+json"


def _token() -> str:
    return get_secret("github_token") or ""


def _auth_headers() -> dict[str, str]:
    headers = {"Accept": _ACCEPT, "X-GitHub-Api-Version": "2022-11-28"}
    token = _token()
    if token:
        headers["Authorization"] = f"Bearer {token}"
    return headers


class GitHubEngine(JsonApiEngine):
    """GitHub repository + issue search (keyless, optional token)."""

    name = "github"
    categories = frozenset({"github"})

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 100))
        params = [f"q={quote_plus(query)}", f"per_page={n}"]
        if filters and filters.freshness:
            params.append("sort=updated&order=desc")
        return f"{_API}/search/repositories?{'&'.join(params)}"

    def _issues_url(self, query: str, max_results: int) -> str:
        n = max(1, min(max_results, 100))
        return f"{_API}/search/issues?q={quote_plus(query)}&per_page={n}"

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        """Repositories first, then issues/PRs to fill the remaining budget.

        Sequential rather than concurrent on purpose: GitHub's anonymous search
        limit is 10 requests/minute *per IP* and it counts both calls, so firing
        them together just hits the wall twice as fast. If repos already filled
        the budget, the second call is skipped entirely.
        """
        headers = _auth_headers()
        repos = await self._get_json(
            self.build_url(query, max_results, filters), headers=headers
        )
        results = self._map_repos(repos)
        if len(results) >= max_results:
            return results
        issues = await self._get_json(
            self._issues_url(query, max_results - len(results)), headers=headers
        )
        results.extend(self._map_issues(issues))
        return results

    def map_results(self, payload: Any) -> list[SearchResult]:
        return self._map_repos(payload)

    @staticmethod
    def _items(payload: Any) -> list[Any]:
        if not isinstance(payload, dict):
            return []
        items = payload.get("items")
        return items if isinstance(items, list) else []

    def _map_repos(self, payload: Any) -> list[SearchResult]:
        results: list[SearchResult] = []
        for item in self._items(payload):
            if not isinstance(item, dict):
                continue
            name = item.get("full_name")
            url = item.get("html_url")
            if not isinstance(name, str) or not isinstance(url, str) or not url:
                continue
            bits = []
            stars = item.get("stargazers_count")
            if isinstance(stars, int):
                bits.append(f"★{stars}")
            lang = item.get("language")
            if isinstance(lang, str) and lang:
                bits.append(lang)
            desc = item.get("description")
            if isinstance(desc, str) and desc:
                bits.append(desc)
            # pushed_at is last-push, which is what "is this alive?" means for
            # a repo — more useful than created_at for freshness.
            pushed = iso_date(item.get("pushed_at"))
            results.append(
                SearchResult(
                    title=name,
                    url=url,
                    snippet=clip(" · ".join(bits)),
                    engine=self.name,
                    rank=0,
                    published_age=pushed,
                    published_age_confident=bool(pushed),
                )
            )
        return results

    def _map_issues(self, payload: Any) -> list[SearchResult]:
        results: list[SearchResult] = []
        for item in self._items(payload):
            if not isinstance(item, dict):
                continue
            title = item.get("title")
            url = item.get("html_url")
            if not isinstance(title, str) or not isinstance(url, str) or not url:
                continue
            bits = []
            # The search/issues endpoint returns PRs too; say which it is.
            kind = "PR" if isinstance(item.get("pull_request"), dict) else "issue"
            state = item.get("state")
            bits.append(f"{kind} {state}" if isinstance(state, str) else kind)
            comments = item.get("comments")
            if isinstance(comments, int) and comments:
                bits.append(f"{comments} comments")
            body = item.get("body")
            if isinstance(body, str) and body:
                bits.append(body)
            created = iso_date(item.get("created_at"))
            results.append(
                SearchResult(
                    title=clip(title, cap=300),
                    url=url,
                    snippet=clip(" · ".join(bits)),
                    engine=self.name,
                    rank=0,
                    published_age=created,
                    published_age_confident=bool(created),
                )
            )
        return results


class GitHubCodeEngine(JsonApiEngine):
    """GitHub code-content search — requires a token (GitHub 401s anonymously)."""

    name = "github_code"
    categories = frozenset({"github"})

    def is_available(self) -> bool:
        # Without a token every call 401s, so keep it out of category routing
        # rather than adding a guaranteed error to every `category="github"`
        # search. Asking for it by name still raises the actionable message.
        return bool(_token())

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 100))
        return f"{_API}/search/code?q={quote_plus(query)}&per_page={n}"

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        if not _token():
            raise EngineKeyError(
                "github_code: GitHub's code search API rejects anonymous requests. "
                "Add a personal access token in the admin UI "
                "(uv run search-mcp-admin) or set SEARCH_MCP_GITHUB_TOKEN. "
                "The keyless `github` engine searches repositories and issues "
                "without one."
            )
        url = self.build_url(query, max_results, filters)
        body = await self._request_text(url, headers=_auth_headers())
        if body is None:
            # _request_text swallows the status, so re-probe cheaply for the
            # auth/quota case rather than reporting a silent empty result.
            raise_for_key_error(self.name, await self._status(url))
            return []
        import json as _json

        try:
            return self.map_results(_json.loads(body))
        except ValueError:
            return []

    async def _status(self, url: str) -> int | None:
        """Best-effort status probe used only to explain a failed search."""
        from curl_cffi.requests import AsyncSession

        from ..config import settings
        from ..httpfetch import IMPERSONATE
        from ..net import curl_proxy_kwargs

        try:
            async with AsyncSession(
                impersonate=IMPERSONATE,
                timeout=settings.request_timeout,
                headers=_auth_headers(),
                **curl_proxy_kwargs(self.name),
            ) as client:
                resp = await client.get(url)
                return resp.status_code
        except Exception:
            return None

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        items = payload.get("items")
        if not isinstance(items, list):
            return []
        results: list[SearchResult] = []
        for item in items:
            if not isinstance(item, dict):
                continue
            path = item.get("path")
            url = item.get("html_url")
            if not isinstance(path, str) or not isinstance(url, str) or not url:
                continue
            repo = item.get("repository")
            repo_name = repo.get("full_name") if isinstance(repo, dict) else ""
            results.append(
                SearchResult(
                    title=f"{repo_name}/{path}" if repo_name else path,
                    url=url,
                    snippet=clip(repo.get("description") if isinstance(repo, dict) else ""),
                    engine=self.name,
                    rank=0,
                )
            )
        return results
