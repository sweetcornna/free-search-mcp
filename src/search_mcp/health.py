"""Which engines are worth asking right now.

Until 0.12 the default pool was a fixed list, asked in full on every search.
Mojeek answered every request with an ALTCHA captcha for weeks, and every
search still paid for the attempt, still listed `mojeek` among its sources, and
still ran on one engine fewer than it claimed. A circuit breaker is the
ordinary answer: an engine that keeps failing is benched for a while, a reserve
takes its seat if the pool gets thin, and one search after the cooldown is the
probe that lets it back in.

State lives in memory, and an open circuit is also written to
`<cache_dir>/engine_health.json` so that the next process starts benched too.
Measured 2026-09-21: Mojeek took 2.6 to 6.2 s to serve its captcha page, and
the pool waits for its slowest member, so a fresh process paid that on its
first search every time. The plugin's server is long-lived and paid it once a
session; `search-mcp ask` and the child servers the answer agent starts are
new processes every run and paid it every run. The file is advisory: a missing
or unreadable one costs one failed attempt, as before.
"""

from __future__ import annotations

import json
import logging
import os
import time
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

from .config import settings

log = logging.getLogger(__name__)

# A wall the engine put up on purpose. One sighting is enough: a captcha does
# not go away on the next request, and asking again is how an IP gets a longer
# ban. `off_topic` belongs here too — a decoy page is a block with better
# manners.
HARD_REASONS = frozenset(
    {"captcha", "consent", "javascript", "login", "no_live_instance", "off_topic"}
)

# Failures that are often a blip: a timeout, a reset, a 503. Two in a row.
SOFT_THRESHOLD = 2
# How long the file keeps an engine whose cooldown has run out, so that the
# escalation (the doubled cooldown on the next failure) survives a restart.
_REMEMBER_EXPIRED_FOR = 24 * 3600.0
# An engine returning nothing, with no error and no wall, is weak evidence on
# its own — some queries have no results. Three in a row, and the aggregator
# only reports it when a peer DID find something for the same query.
SILENT_THRESHOLD = 3


@dataclass
class _State:
    strikes: int = 0
    trips: int = 0
    opened_at: float | None = None
    cooldown: float = 0.0
    reason: str = ""


class EngineHealth:
    """A per-engine circuit breaker.

    `cooldown` / `max_cooldown` default to the settings, read at the moment a
    circuit opens, so an operator's override (and a test's monkeypatch) applies
    without rebuilding the singleton. `clock` (monotonic, for the durations in
    memory) and `wall` (for the expiry written to the file, which another
    process has to read) are injectable for the same reason tests exist.
    """

    def __init__(
        self,
        cooldown: float | None = None,
        max_cooldown: float | None = None,
        clock: Callable[[], float] = time.monotonic,
        wall: Callable[[], float] = time.time,
    ) -> None:
        self._cooldown = cooldown
        self._max_cooldown = max_cooldown
        self._clock = clock
        self._wall = wall
        self._states: dict[str, _State] = {}
        self._loaded = False

    def reset(self) -> None:
        """Forget everything, on disk as well: a reset that a later call undid
        by reading the file back would not be one."""
        self._states.clear()
        try:
            self._path().unlink()
        except OSError:
            pass
        # Read again on the next call, from wherever `cache_dir` points by then.
        self._loaded = False

    # --- the file ---------------------------------------------------------

    @staticmethod
    def _path() -> Path:
        return settings.cache_dir / "engine_health.json"

    def _load(self) -> None:
        """Adopt the open circuits another process left behind, once."""
        if self._loaded:
            return
        self._loaded = True
        try:
            rows = json.loads(self._path().read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return
        if not isinstance(rows, dict):
            return
        now = self._wall()
        for name, row in rows.items():
            if name in self._states or not isinstance(row, dict):
                continue
            try:
                remaining = float(row["until"]) - now
                cooldown = float(row["cooldown"])
            except (KeyError, TypeError, ValueError):
                continue
            if cooldown <= 0 or remaining <= -_REMEMBER_EXPIRED_FOR:
                continue
            # Wall-clock expiry from the file, monotonic clock in memory: the
            # circuit is placed so that it has `remaining` seconds left here.
            # An expired one is adopted closed, with its trips, so the probe
            # that fails again gets the doubled cooldown as it would have in
            # one process.
            self._states[name] = _State(
                strikes=int(row.get("strikes") or 1),
                trips=int(row.get("trips") or 1),
                opened_at=self._clock() - (cooldown - max(0.0, min(remaining, cooldown))),
                cooldown=cooldown,
                reason=str(row.get("reason") or "error"),
            )

    def _save(self) -> None:
        """Write the open circuits. Last writer wins, and that is fine: every
        writer has just observed the engine itself."""
        rows: dict[str, dict[str, object]] = {}
        now = self._wall()
        for name, state in self._states.items():
            if state.opened_at is None:
                continue
            remaining = state.cooldown - (self._clock() - state.opened_at)
            if remaining <= -_REMEMBER_EXPIRED_FOR:
                continue
            rows[name] = {
                "until": now + remaining,
                "cooldown": state.cooldown,
                "reason": state.reason,
                "strikes": state.strikes,
                "trips": state.trips,
            }
        path = self._path()
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            tmp = path.with_name(f"{path.name}.{os.getpid()}.tmp")
            tmp.write_text(json.dumps(rows, indent=1), encoding="utf-8")
            os.replace(tmp, path)
        except OSError as exc:
            log.debug("engine health not saved: %s", exc)

    # --- the breaker ------------------------------------------------------

    def record_success(self, name: str) -> None:
        # Success clears everything, including the escalation: an engine that
        # came back is given the short cooldown again if it fails next month.
        self._load()
        state = self._states.pop(name, None)
        if state is not None and state.opened_at is not None:
            # It was benched in the file too, and it is back.
            self._save()

    def record_failure(self, name: str, reason: str, *, threshold: int | None = None) -> None:
        """Count one failed run; open the circuit once `threshold` is reached.

        `threshold` defaults by reason: 1 for a deliberate wall, otherwise
        `SOFT_THRESHOLD`. Strikes are consecutive and survive the cooldown, so
        the probe after a bench re-opens the circuit on its first failure —
        with the cooldown doubled, up to the maximum.
        """
        self._load()
        if threshold is None:
            threshold = 1 if reason in HARD_REASONS else SOFT_THRESHOLD
        state = self._states.setdefault(name, _State())
        state.strikes += 1
        if state.strikes < threshold:
            return
        base = self._cooldown if self._cooldown is not None else settings.engine_cooldown_seconds
        cap = (
            self._max_cooldown
            if self._max_cooldown is not None
            else settings.engine_cooldown_max_seconds
        )
        state.cooldown = min(base * (2**state.trips), cap)
        state.trips += 1
        state.opened_at = self._clock()
        state.reason = reason
        self._save()

    def is_open(self, name: str) -> bool:
        """True while the engine is benched. False again once the cooldown has
        run out — the next search that includes it is the probe."""
        self._load()
        state = self._states.get(name)
        if state is None or state.opened_at is None:
            return False
        return self._clock() - state.opened_at < state.cooldown

    def describe(self, name: str) -> dict[str, object] | None:
        """`{"reason", "retry_in_seconds"}` for a benched engine, else None."""
        self._load()
        if not self.is_open(name):
            return None
        state = self._states[name]
        assert state.opened_at is not None
        remaining = state.cooldown - (self._clock() - state.opened_at)
        return {"reason": state.reason, "retry_in_seconds": max(1, round(remaining))}


engine_health = EngineHealth()
