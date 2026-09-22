"""GitHub releases: the latest release of the repositories a query names.

  GET https://api.github.com/search/repositories?q=<q>&per_page=3
  GET https://api.github.com/repos/<owner>/<repo>/releases/latest

Two round trips, so the first is skipped when the query already contains an
`owner/repo`. The same optional token as the `github` engine applies; without
one GitHub allows 10 searches a minute and 60 other calls an hour from one
address, which is enough for a person and not for a crowd.
"""

from __future__ import annotations

import asyncio
import re
from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .facts import STOP_WORDS, candidate_names, iso_day, version_question
from .github import _API, _auth_headers
from .jsonapi import JsonApiEngine, clip

_REPO_RE = re.compile(r"\b([A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?)/([A-Za-z0-9._-]+)\b")


def _names_repo(item: dict[str, Any], names: list[str]) -> bool:
    """Whether the query's candidate names name this repository.

    Repository search is fuzzy: "python 3.9" finds Transcrypt v3.9. A release
    record for the wrong project is worse than none, so a hit counts only when
    its name matches a candidate, or the candidates joined ("vs code" and
    vscode), ignoring case, dots, dashes and underscores.
    """
    plain = _plain(str(item.get("name") or ""))
    wanted = {_plain(n) for n in names}
    if len(names) > 1:
        wanted.add("".join(_plain(n) for n in names))
    return plain in wanted


def _plain(name: str) -> str:
    return re.sub(r"[-_.]", "", name.lower())


class GitHubReleasesEngine(JsonApiEngine):
    """Latest GitHub release of the repositories the query names or describes."""

    name = "github_releases"
    single_site = True
    categories = frozenset({"software", "software.github"})
    direct_answer = True
    description = "GitHub releases: the newest release tag, date and notes of a repository."

    def claims(self, query: str) -> bool:
        # Only when a repository is named or GitHub is: the unauthenticated
        # API allows 60 requests an hour, too few to spend on every version
        # question.
        return bool(_REPO_RE.search(query)) or (
            version_question(query) and bool(re.search(r"github|release", query, re.I))
        )

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        terms = " ".join(w for w in query.split() if w.lower() not in STOP_WORDS) or query
        return f"{_API}/search/repositories?q={quote_plus(terms)}&per_page=3"

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        headers = _auth_headers()
        repos = [f"{m.group(1)}/{m.group(2)}" for m in _REPO_RE.finditer(query)][:2]
        if not repos:
            names = candidate_names(query, limit=3)
            if not names:
                return []
            url = self.build_url(query, max_results, filters)
            found = await self._get_json(url, headers=headers)
            items = found.get("items") if isinstance(found, dict) else None
            for item in items or []:
                if not isinstance(item, dict) or not isinstance(item.get("full_name"), str):
                    continue
                if _names_repo(item, names):
                    repos.append(item["full_name"])
            repos = repos[:2]
        payloads = await asyncio.gather(
            *(
                self._get_json(f"{_API}/repos/{repo}/releases/latest", headers=headers)
                for repo in repos
            )
        )
        results: list[SearchResult] = []
        for repo, payload in zip(repos, payloads, strict=True):
            results.extend(self._map_release(repo, payload))
        return results

    def map_results(self, payload: Any) -> list[SearchResult]:
        return self._map_release("", payload)

    def _map_release(self, repo: str, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict) or not isinstance(payload.get("tag_name"), str):
            return []
        tag = payload["tag_name"]
        url = payload.get("html_url")
        if not isinstance(url, str) or not url.startswith("https://github.com/"):
            return []
        repo = repo or "/".join(url.split("/")[3:5])
        published = iso_day(payload.get("published_at"))
        bits = [f"Latest release {tag}" + (f", published {published}" if published else "")]
        if payload.get("prerelease"):
            bits.append("marked as a pre-release")
        release_name = payload.get("name")
        if isinstance(release_name, str) and release_name and release_name != tag:
            bits.append(release_name)
        body = payload.get("body")
        if isinstance(body, str) and body.strip():
            # Release notes are markdown; headings, bullets and commit hashes
            # cost snippet space without saying anything.
            plain = re.sub(r"\b[0-9a-f]{40}\b", "", body)
            plain = re.sub(r"[#*`>_\[\]()-]+", " ", plain)
            bits.append(" ".join(plain.split()))
        return [
            SearchResult(
                title=f"{repo} {tag}: latest release on GitHub",
                url=url,
                snippet=clip(" · ".join(bits)),
                engine=self.name,
                rank=0,
                published_age=published,
                published_age_confident=bool(published),
            )
        ]
