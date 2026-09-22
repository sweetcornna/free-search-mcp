"""OSV: known vulnerabilities in a package, by ecosystem and name.

  POST https://api.osv.dev/v1/query  {"package": {"purl": "pkg:pypi/requests"}}

OSV aggregates PyPA, GitHub Security Advisories, RustSec, Go and the other
ecosystem databases under one schema. Asked about a package name it returns
every advisory that touches any version, each with the affected ranges, so
an agent can say "fixed in 2.32.4" from the record instead of from a blog.
The ecosystem is guessed from the query (pip, npm, cargo, go); with no hint
both PyPI and npm are asked, because a bare name is most often one of them.
"""

from __future__ import annotations

import asyncio
import re
from typing import Any
from urllib.parse import quote

from .base import SearchFilters, SearchResult
from .facts import _TOKEN_RE, candidate_names, iso_day
from .jsonapi import JsonApiEngine, clip

_API = "https://api.osv.dev/v1/query"
_HINTS = {
    "pypi": "pypi",
    "pip": "pypi",
    "python": "pypi",
    "npm": "npm",
    "node": "npm",
    "nodejs": "npm",
    "javascript": "npm",
    "cargo": "cargo",
    "crate": "cargo",
    "crates": "cargo",
    "rust": "cargo",
    "go": "golang",
    "golang": "golang",
    "maven": "maven",
    "java": "maven",
    "gem": "rubygems",
    "ruby": "rubygems",
    "rubygems": "rubygems",
    "composer": "packagist",
    "php": "packagist",
    "nuget": "nuget",
    "dotnet": "nuget",
}
_DEFAULT_ECOSYSTEMS = ("pypi", "npm")
_VULN_ASK = re.compile(
    r"\b(vulnerabilit(?:y|ies)|advisor(?:y|ies)|cve|security (?:issue|bug|fix)|exploit)\b|"
    r"漏洞|安全公告|安全问题",
    re.I,
)
_ID_RE = re.compile(r"\b(?:CVE|GHSA|PYSEC|RUSTSEC|GO|OSV)-[0-9A-Za-z-]{4,}\b", re.I)


def _canonical_id(vuln_id: str) -> str:
    """Upper-case the prefix only: a GHSA suffix is lower-case and case-sensitive."""
    prefix, sep, rest = vuln_id.partition("-")
    if prefix.upper() in ("CVE", "PYSEC", "RUSTSEC", "GO", "OSV"):
        return vuln_id.upper()
    return prefix.upper() + sep + rest


def ecosystems_for(query: str) -> tuple[str, ...]:
    """The package ecosystems a query names, or the two most common ones."""
    found: list[str] = []
    for token in _TOKEN_RE.findall(query.lower()):
        eco = _HINTS.get(token)
        if eco and eco not in found:
            found.append(eco)
    return tuple(found) or _DEFAULT_ECOSYSTEMS


class OsvEngine(JsonApiEngine):
    """OSV advisories for the packages a query names."""

    name = "osv"
    single_site = True
    categories = frozenset({"security", "security.package"})
    impersonate = None
    direct_answer = True
    description = "OSV.dev: known vulnerabilities and fixed versions for a package (PyPI, npm...)."

    def claims(self, query: str) -> bool:
        if _ID_RE.search(query):
            return True
        return bool(_VULN_ASK.search(query)) and bool(
            [n for n in candidate_names(query, limit=4) if n not in _HINTS]
        )

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        return _API

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        ids = _ID_RE.findall(query)
        if ids:
            payloads = await asyncio.gather(
                *(
                    self._get_json(f"https://api.osv.dev/v1/vulns/{quote(_canonical_id(i))}")
                    for i in ids[:2]
                )
            )
            found = [p for p in payloads if isinstance(p, dict) and p.get("id")]
            return self._rank(self._map_many(found, package=""), max_results)
        names = [n for n in candidate_names(query, limit=4) if n not in _HINTS][:2]
        if not names:
            return []
        purls = [f"pkg:{eco}/{name}" for name in names for eco in ecosystems_for(query)]
        payloads = await asyncio.gather(
            *(
                self._get_json(_API, method="POST", json_body={"package": {"purl": p}})
                for p in purls
            )
        )
        results: list[SearchResult] = []
        for purl, payload in zip(purls, payloads, strict=True):
            vulns = payload.get("vulns") if isinstance(payload, dict) else None
            if isinstance(vulns, list) and vulns:
                results.extend(self._map_many(vulns, package=purl))
        return self._rank(results, max_results)

    def map_results(self, payload: Any) -> list[SearchResult]:
        vulns = payload.get("vulns") if isinstance(payload, dict) else None
        return self._map_many(vulns if isinstance(vulns, list) else [], package="")

    @staticmethod
    def _rank(results: list[SearchResult], max_results: int) -> list[SearchResult]:
        # Newest advisory first: that is the one a "is X vulnerable" question is about.
        results.sort(key=lambda r: r.published_age or "", reverse=True)
        return [
            SearchResult(
                title=r.title,
                url=r.url,
                snippet=r.snippet,
                engine=r.engine,
                rank=i,
                published_age=r.published_age,
                published_age_confident=r.published_age_confident,
            )
            for i, r in enumerate(results[:max_results])
        ]

    def _map_many(self, vulns: list[Any], *, package: str) -> list[SearchResult]:
        out: list[SearchResult] = []
        for vuln in vulns:
            if not isinstance(vuln, dict) or not isinstance(vuln.get("id"), str):
                continue
            vid = vuln["id"]
            aliases = [a for a in vuln.get("aliases") or [] if isinstance(a, str)]
            severity = _severity(vuln)
            when = iso_day(vuln.get("published") or vuln.get("modified"))
            bits = []
            if severity:
                bits.append(severity)
            if aliases:
                bits.append("also " + ", ".join(aliases[:3]))
            affected = _affected(vuln.get("affected"), package)
            if affected:
                bits.append(affected)
            if when:
                bits.append(f"published {when}")
            summary = vuln.get("summary") or vuln.get("details") or ""
            summary = summary.strip() if isinstance(summary, str) else ""
            if summary:
                bits.append(clip(summary, cap=240))
            title = vid + (f": {clip(summary, cap=80)}" if summary else "")
            out.append(
                SearchResult(
                    title=title,
                    url=f"https://osv.dev/vulnerability/{quote(vid)}",
                    snippet=clip(" · ".join(bits), cap=500),
                    engine=self.name,
                    rank=0,
                    published_age=when or None,
                    published_age_confident=bool(when),
                )
            )
        return out


def _severity(vuln: dict[str, Any]) -> str:
    db = vuln.get("database_specific")
    if isinstance(db, dict) and isinstance(db.get("severity"), str):
        return db["severity"].capitalize() + " severity"
    for entry in vuln.get("severity") or []:
        if isinstance(entry, dict) and isinstance(entry.get("score"), str):
            return f"{entry.get('type', 'CVSS')} {entry['score'][:44]}"
    return ""


def _affected(affected: Any, package: str) -> str:
    if not isinstance(affected, list):
        return ""
    for entry in affected:
        if not isinstance(entry, dict):
            continue
        pkg = entry.get("package") if isinstance(entry.get("package"), dict) else {}
        wanted = package.rsplit("/", 1)[-1] if package else ""
        if wanted and pkg.get("purl") not in (package, None) and pkg.get("name") != wanted:
            continue
        label = f"{pkg.get('ecosystem', '')} {pkg.get('name', '')}".strip()
        fixed = [
            e["fixed"]
            for r in entry.get("ranges") or []
            if isinstance(r, dict)
            for e in r.get("events") or []
            if isinstance(e, dict) and isinstance(e.get("fixed"), str)
        ]
        introduced = [
            e["introduced"]
            for r in entry.get("ranges") or []
            if isinstance(r, dict)
            for e in r.get("events") or []
            if isinstance(e, dict) and isinstance(e.get("introduced"), str)
        ]
        parts = []
        if label:
            parts.append(label)
        if introduced and introduced[0] not in ("0", ""):
            parts.append(f"from {introduced[0]}")
        parts.append(f"fixed in {', '.join(fixed[:3])}" if fixed else "no fix listed")
        return " ".join(parts)
    return ""
