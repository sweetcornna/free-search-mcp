"""The circuit breaker, on a clock the test controls."""
from __future__ import annotations

import json
import time

import pytest

from search_mcp.health import HARD_REASONS, SILENT_THRESHOLD, SOFT_THRESHOLD, EngineHealth


class Clock:
    def __init__(self):
        self.now = 1_000.0

    def __call__(self) -> float:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += seconds


@pytest.fixture
def clock():
    return Clock()


@pytest.fixture
def health(clock):
    return EngineHealth(cooldown=600, max_cooldown=3600, clock=clock)


@pytest.mark.parametrize("reason", sorted(HARD_REASONS))
def test_a_wall_benches_the_engine_at_once(health, reason):
    health.record_failure("mojeek", reason)
    assert health.is_open("mojeek")
    assert health.describe("mojeek") == {"reason": reason, "retry_in_seconds": 600}


def test_a_blip_does_not(health):
    health.record_failure("bing", "error")
    assert not health.is_open("bing")
    assert health.describe("bing") is None


def test_repeated_blips_do(health):
    for _ in range(SOFT_THRESHOLD):
        health.record_failure("bing", "error")
    assert health.is_open("bing")


def test_a_success_between_blips_starts_the_count_again(health):
    health.record_failure("bing", "error")
    health.record_success("bing")
    health.record_failure("bing", "error")
    assert not health.is_open("bing")


def test_silence_needs_more_evidence_than_an_error(health):
    for i in range(SILENT_THRESHOLD):
        assert not health.is_open("anysearch"), i
        health.record_failure("anysearch", "silent", threshold=SILENT_THRESHOLD)
    assert health.is_open("anysearch")


def test_the_cooldown_runs_out_and_the_next_search_is_the_probe(health, clock):
    health.record_failure("mojeek", "captcha")
    clock.advance(599)
    assert health.describe("mojeek")["retry_in_seconds"] == 1
    clock.advance(2)
    assert not health.is_open("mojeek")


def test_a_failed_probe_doubles_the_cooldown_up_to_the_cap(health, clock):
    expected = [600, 1200, 2400, 3600, 3600]
    for cooldown in expected:
        health.record_failure("mojeek", "captcha")
        assert health.describe("mojeek")["retry_in_seconds"] == cooldown
        clock.advance(cooldown + 1)
        assert not health.is_open("mojeek")


def test_a_failed_probe_re_opens_on_its_first_soft_failure(health, clock):
    """Strikes survive the cooldown: an engine that needed two errors to be
    benched does not get two more free errors every time it is probed."""
    for _ in range(SOFT_THRESHOLD):
        health.record_failure("bing", "error")
    clock.advance(601)
    assert not health.is_open("bing")
    health.record_failure("bing", "error")
    assert health.is_open("bing")


def test_recovery_forgets_the_escalation(health, clock):
    health.record_failure("mojeek", "captcha")
    clock.advance(601)
    health.record_failure("mojeek", "captcha")  # now on the 1200s rung
    clock.advance(1201)
    health.record_success("mojeek")

    health.record_failure("mojeek", "captcha")
    assert health.describe("mojeek")["retry_in_seconds"] == 600


def test_engines_are_independent(health):
    health.record_failure("mojeek", "captcha")
    assert not health.is_open("duckduckgo")


def test_reset_clears_everything(health):
    health.record_failure("mojeek", "captcha")
    health.reset()
    assert not health.is_open("mojeek")


def test_the_cooldown_comes_from_settings_when_not_given(monkeypatch, clock):
    from search_mcp.config import settings

    monkeypatch.setattr(settings, "engine_cooldown_seconds", 42.0)
    health = EngineHealth(clock=clock)
    health.record_failure("mojeek", "captcha")
    assert health.describe("mojeek")["retry_in_seconds"] == 42


# ---------------------------------------------------------------------------
# The file: a fresh process starts benched
# ---------------------------------------------------------------------------


def test_an_open_circuit_is_adopted_by_the_next_process(clock):
    """Two trackers stand in for two processes. The second one has never seen
    Mojeek fail and still leaves it out, with the remaining cooldown."""
    from search_mcp.config import settings

    first = EngineHealth(cooldown=600, max_cooldown=3600, clock=clock, wall=clock)
    first.record_failure("mojeek", "captcha")
    assert (settings.cache_dir / "engine_health.json").exists()

    clock.advance(100)
    second = EngineHealth(cooldown=600, max_cooldown=3600, clock=clock, wall=clock)
    assert second.is_open("mojeek")
    assert second.describe("mojeek") == {"reason": "captcha", "retry_in_seconds": 500}
    clock.advance(501)
    assert not second.is_open("mojeek"), "the probe is due at the same moment in both"


def test_a_recovery_is_written_too(clock):
    first = EngineHealth(cooldown=600, max_cooldown=3600, clock=clock)
    first.record_failure("mojeek", "captcha")
    first.record_success("mojeek")
    second = EngineHealth(cooldown=600, max_cooldown=3600, clock=clock)
    assert not second.is_open("mojeek")


def test_an_expired_or_broken_file_costs_nothing(clock):
    from search_mcp.config import settings

    path = settings.cache_dir / "engine_health.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("{not json")
    assert not EngineHealth(clock=clock).is_open("mojeek")
    path.write_text(json.dumps({"mojeek": {"until": time.time() - 5, "cooldown": 600, "reason": "captcha"}}))
    assert not EngineHealth(clock=clock).is_open("mojeek")
    path.write_text(json.dumps({"mojeek": "nonsense", "bing": {"until": "soon"}}))
    assert not EngineHealth(clock=clock).is_open("bing")


def test_the_escalation_survives_a_restart(clock):
    """Trips are in the file, so the probe after a restart that fails again
    gets the doubled cooldown, as it would have in one process."""
    first = EngineHealth(cooldown=600, max_cooldown=3600, clock=clock, wall=clock)
    first.record_failure("mojeek", "captcha")
    clock.advance(601)
    second = EngineHealth(cooldown=600, max_cooldown=3600, clock=clock, wall=clock)
    assert not second.is_open("mojeek")
    second.record_failure("mojeek", "captcha")
    assert second.describe("mojeek")["retry_in_seconds"] == 1200
