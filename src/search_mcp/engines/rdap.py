"""RDAP: who registered a domain, when, and when it expires.

  GET https://data.iana.org/rdap/dns.json            (the bootstrap, kept a day)
  GET https://rdap.verisign.com/com/v1/domain/example.com

RDAP is the registries' own successor to WHOIS, JSON over HTTPS, keyless.
IANA's bootstrap file says which registry serves which top-level domain, so
each lookup goes to the registry of record rather than through a third
party. The record carries the registration, expiry and last-changed events,
the registrar and the name servers.
"""

from __future__ import annotations

import re
import time
from typing import Any

from .base import SearchFilters, SearchResult
from .facts import iso_day
from .jsonapi import JsonApiEngine, clip

_BOOTSTRAP = "https://data.iana.org/rdap/dns.json"
_DOMAIN = re.compile(
    r"\b((?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+(?:[a-z]{2,24}|xn--[a-z0-9-]+))\b", re.I
)
_DOMAIN_ASK = re.compile(
    r"\b(whois|rdap|registrar|registered|registration|expir(?:es?|y|ation)|domain|"
    r"name ?servers?|dns)\b|域名|注册商|注册时间|到期|过期|续费|whois",
    re.I,
)
_NOT_DOMAINS = frozenset({"e.g", "i.e", "vs", "etc"})
_TTL = 24 * 3600.0
_bootstrap: dict[str, Any] = {"at": 0.0, "map": {}}


def domains_in(query: str, *, limit: int = 2) -> list[str]:
    found: list[str] = []
    for m in _DOMAIN.finditer(query):
        name = m.group(1).lower().strip(".")
        if name in _NOT_DOMAINS or name in found:
            continue
        # Bare hostnames with a path or scheme in the question are still
        # domains; a version number like 1.2.3 is not (its "tld" is digits).
        found.append(name)
    return found[:limit]


class RdapEngine(JsonApiEngine):
    """Domain registration records from the registry's RDAP service."""

    name = "rdap"
    single_site = True
    categories = frozenset({"reference", "reference.domain"})
    impersonate = None
    direct_answer = True
    description = "RDAP: a domain's registration date, expiry, registrar and name servers."

    def claims(self, query: str) -> bool:
        return bool(_DOMAIN_ASK.search(query)) and bool(domains_in(query))

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        return _BOOTSTRAP

    async def _registry_for(self, tld: str) -> str | None:
        if time.monotonic() - _bootstrap["at"] > _TTL or not _bootstrap["map"]:
            payload = await self._get_json(_BOOTSTRAP)
            services = payload.get("services") if isinstance(payload, dict) else None
            table: dict[str, str] = {}
            for entry in services or []:
                if not isinstance(entry, list) or len(entry) < 2:
                    continue
                tlds, urls = entry[0], entry[1]
                https = [u for u in urls if isinstance(u, str) and u.startswith("https://")]
                if not https:
                    continue
                for t in tlds:
                    if isinstance(t, str):
                        table[t.lower()] = https[0].rstrip("/") + "/"
            if table:
                _bootstrap["map"] = table
                _bootstrap["at"] = time.monotonic()
        return _bootstrap["map"].get(tld)

    async def fetch_results(
        self, query: str, max_results: int, filters: SearchFilters | None
    ) -> list[SearchResult]:
        results: list[SearchResult] = []
        for domain in domains_in(query):
            # Registrable name: the last two labels, or three under a
            # two-level public suffix such as co.uk or com.cn.
            labels = domain.split(".")
            if len(labels) >= 3 and labels[-2] in ("co", "com", "net", "org", "gov", "ac", "edu"):
                domain = ".".join(labels[-3:])
            else:
                domain = ".".join(labels[-2:])
            base = await self._registry_for(domain.rsplit(".", 1)[-1])
            if not base:
                continue
            payload = await self._get_json(f"{base}domain/{domain}")
            if isinstance(payload, dict):
                payload["_request"] = f"{base}domain/{domain}"
            results.extend(self.map_results(payload))
        return results[:max_results]

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict) or not isinstance(payload.get("ldhName"), str):
            return []
        name = payload["ldhName"].lower()
        events = {
            str(e.get("eventAction")): iso_day(e.get("eventDate"))
            for e in payload.get("events") or []
            if isinstance(e, dict)
        }
        registered = events.get("registration", "")
        expires = events.get("expiration", "")
        changed = events.get("last changed", "")
        registrar = ""
        for entity in payload.get("entities") or []:
            if not isinstance(entity, dict) or "registrar" not in (entity.get("roles") or []):
                continue
            vcard = entity.get("vcardArray")
            if isinstance(vcard, list) and len(vcard) > 1:
                for field in vcard[1]:
                    if isinstance(field, list) and field and field[0] == "fn" and len(field) > 3:
                        registrar = str(field[3])
            break
        servers = [
            str(ns["ldhName"]).lower()
            for ns in payload.get("nameservers") or []
            if isinstance(ns, dict) and isinstance(ns.get("ldhName"), str)
        ]
        status = [s for s in payload.get("status") or [] if isinstance(s, str)]
        bits = []
        if registered:
            bits.append(f"registered {registered}")
        if expires:
            bits.append(f"expires {expires}")
        if changed:
            bits.append(f"last changed {changed}")
        if registrar:
            bits.append(f"registrar {registrar}")
        if status:
            bits.append("status " + ", ".join(status[:4]))
        if servers:
            bits.append("name servers " + ", ".join(servers[:4]))
        bits.append("RDAP record from the registry")
        head = f"{name}: registered {registered}" if registered else name
        if expires:
            head += f", expires {expires}"
        return [
            SearchResult(
                title=head,
                url=str(payload.get("_request") or f"https://rdap.org/domain/{name}"),
                snippet=clip(" · ".join(bits)),
                engine=self.name,
                rank=0,
                published_age=changed or None,
                published_age_confident=bool(changed),
            )
        ]
