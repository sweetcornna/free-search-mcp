"""ClinicalTrials.gov — the registry of human clinical studies. Keyless.

  GET https://clinicaltrials.gov/api/v2/studies?query.term=<q>&format=json

Registered trials are primary evidence that the published literature often has
not caught up with: a phase-3 study that is still recruiting has no paper, and a
completed one that was never written up leaves no trace in `pubmed` at all.
Registration also predates results, so this is where the null results and the
abandoned programmes are visible.

Each result carries the phase, recruitment status, enrolment and sponsor up
front, because "what stage is this drug at" is usually the actual question.
"""

from __future__ import annotations

from typing import Any
from urllib.parse import quote_plus

from .base import SearchFilters, SearchResult
from .jsonapi import JsonApiEngine, clip

_ENDPOINT = "https://clinicaltrials.gov/api/v2/studies"
_STUDY = "https://clinicaltrials.gov/study/{nct_id}"


class ClinicalTrialsEngine(JsonApiEngine):
    """ClinicalTrials.gov study search (keyless JSON API v2)."""

    name = "clinicaltrials"
    description = "ClinicalTrials.gov: registered human trials with phase, status and sponsor."
    categories = frozenset({"paper", "paper.trial"})

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        n = max(1, min(max_results, 100))
        params = [
            f"query.term={quote_plus(query)}",
            f"pageSize={n}",
            "format=json",
            # Only the modules actually rendered — the default study record is
            # tens of KB per hit (eligibility criteria, every outcome measure,
            # every site's contact details).
            "fields=protocolSection.identificationModule,"
            "protocolSection.statusModule,"
            "protocolSection.designModule,"
            "protocolSection.conditionsModule,"
            "protocolSection.sponsorCollaboratorsModule,"
            "protocolSection.descriptionModule",
        ]
        if filters and filters.freshness:
            params.append("sort=LastUpdatePostDate:desc")
        return f"{_ENDPOINT}?{'&'.join(params)}"

    def map_results(self, payload: Any) -> list[SearchResult]:
        if not isinstance(payload, dict):
            return []
        studies = payload.get("studies")
        if not isinstance(studies, list):
            return []

        results: list[SearchResult] = []
        for study in studies:
            section = study.get("protocolSection") if isinstance(study, dict) else None
            if not isinstance(section, dict):
                continue
            ident = self._module(section, "identificationModule")
            nct_id = ident.get("nctId")
            title = ident.get("briefTitle") or ident.get("officialTitle")
            if not isinstance(nct_id, str) or not nct_id:
                continue
            if not isinstance(title, str) or not title.strip():
                continue
            date = self._start_date(self._module(section, "statusModule"))
            results.append(
                SearchResult(
                    title=clip(f"{nct_id} — {title}", cap=300),
                    url=_STUDY.format(nct_id=quote_plus(nct_id)),
                    snippet=self._snippet(section),
                    engine=self.name,
                    rank=0,
                    published_age=date,
                    # A registry start date, recorded as a date. Note it can be
                    # in the FUTURE for a study that has not begun; the shared
                    # age helper clamps future dates to "now" rather than
                    # producing a negative age.
                    published_age_confident=bool(date),
                )
            )
        return results

    @staticmethod
    def _module(section: dict[str, Any], name: str) -> dict[str, Any]:
        block = section.get(name)
        return block if isinstance(block, dict) else {}

    @staticmethod
    def _start_date(status: dict[str, Any]) -> str:
        struct = status.get("startDateStruct")
        date = struct.get("date") if isinstance(struct, dict) else None
        # The registry allows a month-precision date ("2024-02"); the shared
        # ISO matcher will not parse that, so it stays display-only.
        return date if isinstance(date, str) and date else ""

    def _snippet(self, section: dict[str, Any]) -> str:
        status = self._module(section, "statusModule")
        design = self._module(section, "designModule")
        conditions = self._module(section, "conditionsModule")
        sponsors = self._module(section, "sponsorCollaboratorsModule")

        bits: list[str] = []
        phases = design.get("phases")
        if isinstance(phases, list):
            named = [p for p in phases if isinstance(p, str) and p and p != "NA"]
            if named:
                bits.append("/".join(named).replace("PHASE", "Phase "))
        overall = status.get("overallStatus")
        if isinstance(overall, str) and overall:
            bits.append(overall.replace("_", " ").title())
        enrolment = design.get("enrollmentInfo")
        count = enrolment.get("count") if isinstance(enrolment, dict) else None
        if isinstance(count, int):
            bits.append(f"n={count}")
        names = conditions.get("conditions")
        if isinstance(names, list):
            listed = [c for c in names if isinstance(c, str) and c]
            if listed:
                bits.append(", ".join(listed[:3]))
        lead = sponsors.get("leadSponsor")
        sponsor = lead.get("name") if isinstance(lead, dict) else None
        if isinstance(sponsor, str) and sponsor:
            bits.append(sponsor)

        head = " · ".join(bits)
        description = self._module(section, "descriptionModule")
        summary = description.get("briefSummary")
        summary = summary if isinstance(summary, str) else ""
        if head and summary:
            return clip(f"{head} — {summary}")
        return clip(summary or head)
