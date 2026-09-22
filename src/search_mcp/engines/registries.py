"""The other package registries, one engine: Maven Central, RubyGems, the Go
module proxy, Homebrew, Docker Hub, Packagist and NuGet.

  GET https://search.maven.org/solrsearch/select?q=<name>&rows=3&wt=json
  GET https://rubygems.org/api/v1/gems/<name>.json
  GET https://proxy.golang.org/<module>/@latest
  GET https://formulae.brew.sh/api/formula/<name>.json
  GET https://hub.docker.com/v2/repositories/<namespace>/<name>
  GET https://repo.packagist.org/p2/<vendor>/<name>.json
  GET https://azuresearch-usnc.nuget.org/query?q=<name>&take=3

Each answers with the current version and its date. The ecosystem is read
from the question (java, gem, go, brew, docker, composer, nuget); with none
named nothing is asked, because a bare name would cost seven requests for
one hit. PyPI, npm and crates.io have engines of their own.
"""

from __future__ import annotations

import asyncio
import re
from typing import Any
from urllib.parse import quote, quote_plus

from .base import SearchFilters, SearchResult
from .facts import candidate_names, ecosystem_hints, iso_day, version_question
from .jsonapi import USER_AGENT, JsonApiEngine, clip

_GO_MODULE = re.compile(r"\b((?:github\.com|gitlab\.com|golang\.org|google\.golang\.org|"
                        r"gopkg\.in|go\.uber\.org|k8s\.io|sigs\.k8s\.io)/[\w./-]+)")
_VENDOR_NAME = re.compile(r"\b([\w.-]+)/([\w.-]+)\b")
_EXTRA_STOP = frozenset(
    {"maven", "gradle", "java", "kotlin", "jar", "gem", "ruby", "bundler", "go", "golang",
     "brew", "homebrew", "formula", "docker", "dockerhub", "image", "container", "composer",
     "packagist", "php", "nuget", "dotnet", ".net", "c#", "csharp", "镜像", "容器"}
)  # fmt: skip


def _names(query: str) -> list[str]:
    return [n for n in candidate_names(query, limit=4) if n not in _EXTRA_STOP][:2]


class RegistriesEngine(JsonApiEngine):
    """Current version and date from Maven, RubyGems, Go, Homebrew, Docker Hub,
    Packagist and NuGet, chosen by the ecosystem the question names."""

    name = "registries"
    single_site = True
    categories = frozenset({"software", "software.registry"})
    impersonate = None
    api_headers = {"User-Agent": USER_AGENT}
    direct_answer = True
    description = (
        "Maven, RubyGems, Go, Homebrew, Docker Hub, Packagist, NuGet: current version and date."
    )

    def claims(self, query: str) -> bool:
        hints = ecosystem_hints(query) - {"pypi", "npm", "cargo", "app"}
        named = bool(_names(query) or _GO_MODULE.search(query))
        return version_question(query) and bool(hints) and named

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        return "https://search.maven.org/solrsearch/select?q=" + quote_plus(" ".join(_names(query)))

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        hints = ecosystem_hints(query) - {"pypi", "npm", "cargo", "app"}
        names = _names(query)
        calls = []
        if "maven" in hints and names:
            calls.append(self._maven(names[0]))
        if "rubygems" in hints:
            calls.extend(self._rubygems(n) for n in names)
        if "go" in hints:
            module = _GO_MODULE.search(query)
            if module:
                calls.append(self._go(module.group(1).rstrip("/.")))
        if "homebrew" in hints:
            calls.extend(self._homebrew(n) for n in names)
        if "docker" in hints:
            calls.extend(self._docker(n) for n in names)
        if "packagist" in hints:
            m = _VENDOR_NAME.search(query)
            if m and "." not in m.group(1):
                calls.append(self._packagist(m.group(1).lower(), m.group(2).lower()))
        if "nuget" in hints and names:
            calls.append(self._nuget(names[0]))
        if not calls:
            return []
        buckets = await asyncio.gather(*calls)
        return [r for bucket in buckets for r in bucket][:max_results]

    def map_results(self, payload: Any) -> list[SearchResult]:
        # The registries differ; fetch_results maps each one. Kept for the
        # base class contract and for a Maven payload handed in directly.
        return self._map_maven(payload)

    def _result(self, title: str, url: str, bits: list[str], day: str) -> SearchResult:
        return SearchResult(
            title=title,
            url=url,
            snippet=clip(" · ".join(b for b in bits if b)),
            engine=self.name,
            rank=0,
            published_age=day or None,
            published_age_confident=bool(day),
        )

    async def _maven(self, name: str) -> list[SearchResult]:
        payload = await self._get_json(
            f"https://search.maven.org/solrsearch/select?q={quote_plus(name)}&rows=3&wt=json"
        )
        return self._map_maven(payload)

    def _map_maven(self, payload: Any) -> list[SearchResult]:
        docs = (payload.get("response") or {}).get("docs") if isinstance(payload, dict) else None
        out = []
        for doc in docs or []:
            if not isinstance(doc, dict) or not isinstance(doc.get("latestVersion"), str):
                continue
            g, a, v = doc.get("g"), doc.get("a"), doc["latestVersion"]
            stamp = doc.get("timestamp")
            day = iso_day(stamp / 1000) if isinstance(stamp, (int, float)) else ""
            out.append(
                self._result(
                    f"{g}:{a} {v} on Maven Central",
                    f"https://central.sonatype.com/artifact/{g}/{a}/{v}",
                    [f"Latest version {v}" + (f", released {day}" if day else ""),
                     f"{doc.get('versionCount', '?')} versions published"],
                    day,
                )
            )
        return out[:2]

    async def _rubygems(self, name: str) -> list[SearchResult]:
        payload = await self._get_json(f"https://rubygems.org/api/v1/gems/{quote(name)}.json")
        if not isinstance(payload, dict) or not isinstance(payload.get("version"), str):
            return []
        day = iso_day(payload.get("version_created_at"))
        bits = [f"Latest version {payload['version']}" + (f", released {day}" if day else "")]
        if isinstance(payload.get("info"), str):
            bits.append(payload["info"])
        home = payload.get("homepage_uri") or payload.get("source_code_uri")
        if isinstance(home, str) and home.startswith("http"):
            bits.append(f"homepage {home}")
        return [
            self._result(
                f"{payload.get('name', name)} {payload['version']} on RubyGems",
                f"https://rubygems.org/gems/{quote(name)}",
                bits,
                day,
            )
        ]

    async def _go(self, module: str) -> list[SearchResult]:
        # The proxy wants upper-case letters escaped as "!x".
        escaped = re.sub(r"[A-Z]", lambda m: "!" + m.group(0).lower(), module)
        payload = await self._get_json(f"https://proxy.golang.org/{escaped}/@latest")
        if not isinstance(payload, dict) or not isinstance(payload.get("Version"), str):
            return []
        day = iso_day(payload.get("Time"))
        return [
            self._result(
                f"{module} {payload['Version']} (Go module)",
                f"https://pkg.go.dev/{module}@{payload['Version']}",
                [f"Latest version {payload['Version']}" + (f", tagged {day}" if day else "")],
                day,
            )
        ]

    async def _homebrew(self, name: str) -> list[SearchResult]:
        payload = await self._get_json(f"https://formulae.brew.sh/api/formula/{quote(name)}.json")
        versions = payload.get("versions") if isinstance(payload, dict) else None
        if not isinstance(versions, dict) or not isinstance(versions.get("stable"), str):
            return []
        bits = [f"Stable version {versions['stable']}"]
        if isinstance(payload.get("desc"), str):
            bits.append(payload["desc"])
        if isinstance(payload.get("license"), str):
            bits.append(f"licence {payload['license']}")
        if isinstance(payload.get("homepage"), str):
            bits.append(f"homepage {payload['homepage']}")
        return [
            self._result(
                f"{payload.get('name', name)} {versions['stable']} on Homebrew",
                f"https://formulae.brew.sh/formula/{quote(name)}",
                bits,
                "",
            )
        ]

    async def _docker(self, name: str) -> list[SearchResult]:
        namespace, _, repo = name.rpartition("/")
        namespace = namespace or "library"
        payload = await self._get_json(
            f"https://hub.docker.com/v2/repositories/{quote(namespace)}/{quote(repo)}"
        )
        if not isinstance(payload, dict) or not isinstance(payload.get("name"), str):
            return []
        day = iso_day(payload.get("last_updated"))
        bits = [f"Last pushed {day}" if day else "Docker Hub repository"]
        if isinstance(payload.get("description"), str) and payload["description"]:
            bits.append(payload["description"])
        pulls = payload.get("pull_count")
        if isinstance(pulls, int):
            bits.append(f"{pulls:,} pulls")
        page = f"https://hub.docker.com/_/{quote(repo)}" if namespace == "library" else (
            f"https://hub.docker.com/r/{quote(namespace)}/{quote(repo)}"
        )
        label = repo if namespace == "library" else f"{namespace}/{repo}"
        return [self._result(f"{label} on Docker Hub", page, bits, day)]

    async def _packagist(self, vendor: str, name: str) -> list[SearchResult]:
        payload = await self._get_json(
            f"https://repo.packagist.org/p2/{quote(vendor)}/{quote(name)}.json"
        )
        packages = payload.get("packages") if isinstance(payload, dict) else None
        versions = packages.get(f"{vendor}/{name}") if isinstance(packages, dict) else None
        if not isinstance(versions, list) or not versions or not isinstance(versions[0], dict):
            return []
        top = versions[0]
        version = top.get("version")
        if not isinstance(version, str):
            return []
        day = iso_day(top.get("time"))
        bits = [f"Latest version {version}" + (f", released {day}" if day else "")]
        if isinstance(top.get("description"), str):
            bits.append(top["description"])
        return [
            self._result(
                f"{vendor}/{name} {version} on Packagist",
                f"https://packagist.org/packages/{quote(vendor)}/{quote(name)}",
                bits,
                day,
            )
        ]

    async def _nuget(self, name: str) -> list[SearchResult]:
        payload = await self._get_json(
            f"https://azuresearch-usnc.nuget.org/query?q={quote_plus(name)}&take=3"
        )
        data = payload.get("data") if isinstance(payload, dict) else None
        out = []
        for pkg in data or []:
            if not isinstance(pkg, dict) or not isinstance(pkg.get("version"), str):
                continue
            pid, version = pkg.get("id", name), pkg["version"]
            bits = [f"Latest version {version}"]
            if isinstance(pkg.get("description"), str):
                bits.append(pkg["description"])
            out.append(
                self._result(
                    f"{pid} {version} on NuGet",
                    f"https://www.nuget.org/packages/{quote(str(pid))}/{quote(version)}",
                    bits,
                    "",
                )
            )
        return out[:2]
