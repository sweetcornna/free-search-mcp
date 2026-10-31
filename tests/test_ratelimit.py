"""TokenBucket hardening: a non-positive rate must mean 'unlimited' rather
than dividing by zero / sleeping a negative duration.
"""
from __future__ import annotations

import asyncio

import pytest

from search_mcp.ratelimit import RateLimiter, TokenBucket

pytestmark = pytest.mark.asyncio


async def test_zero_rate_acquire_does_not_raise_repeatedly():
    """TokenBucket(0).acquire() called multiple times must not raise
    (no ZeroDivisionError, no negative sleep)."""
    bucket = TokenBucket(0)
    # Bound with a timeout so a regression that sleeps forever fails loudly
    # rather than hanging.
    await asyncio.wait_for(bucket.acquire(), timeout=1.0)
    await asyncio.wait_for(bucket.acquire(), timeout=1.0)


async def test_negative_rate_acquire_does_not_raise():
    bucket = TokenBucket(-5)
    await asyncio.wait_for(bucket.acquire(), timeout=1.0)
    await asyncio.wait_for(bucket.acquire(), timeout=1.0)


async def test_zero_rate_acquire_returns_immediately():
    """Unlimited path should not block on a large request either."""
    bucket = TokenBucket(0)
    await asyncio.wait_for(bucket.acquire(100), timeout=1.0)


async def test_positive_rate_still_consumes_tokens():
    """The unlimited shortcut must not break normal positive-rate behavior."""
    bucket = TokenBucket(60)  # 1 token/sec, capacity 60
    start_tokens = bucket.tokens
    await bucket.acquire(1)
    assert bucket.tokens <= start_tokens - 1 + 1e-6


async def test_positive_rate_blocks_when_exhausted():
    """A tiny bucket should force a wait once drained — proves we didn't turn
    every bucket into a no-op."""
    bucket = TokenBucket(60, burst=1)  # capacity 1, refill 1/sec
    await bucket.acquire(1)  # drain
    start = asyncio.get_event_loop().time()
    await asyncio.wait_for(bucket.acquire(1), timeout=5.0)
    elapsed = asyncio.get_event_loop().time() - start
    assert elapsed > 0.1  # had to wait for a refill


async def test_ratelimiter_with_zero_default_does_not_raise():
    limiter = RateLimiter(0)
    await asyncio.wait_for(limiter.acquire("any-key"), timeout=1.0)
    await asyncio.wait_for(limiter.acquire("any-key"), timeout=1.0)


async def test_a_burst_cap_below_the_rate_skips_the_fourth_request():
    from search_mcp.ratelimit import RateLimiter

    limiter = RateLimiter(30)
    limiter.configure("quark", 10, 3)
    assert [await limiter.acquire("quark", max_wait=0) for _ in range(4)] == [
        True, True, True, False,
    ]  # fmt: skip


async def test_quark_is_configured_with_its_burst_cap():
    from search_mcp.aggregator import search_limiter

    bucket = search_limiter._buckets["quark"]
    assert bucket.capacity == 3
    assert bucket.rate == 10 / 60.0


async def test_a_caller_that_will_not_wait_is_not_queued_behind_a_sleeper():
    bucket = TokenBucket(60, burst=1)  # one token, refilled once a second
    await bucket.acquire(1)
    sleeper = asyncio.ensure_future(bucket.acquire(1))  # waits about a second
    await asyncio.sleep(0)
    started = asyncio.get_event_loop().time()
    assert await bucket.acquire(1, max_wait=0) is False
    assert asyncio.get_event_loop().time() - started < 0.05
    assert await sleeper is True


async def test_waiters_are_served_in_arrival_order():
    bucket = TokenBucket(600, burst=1)  # a token every 0.1 s
    await bucket.acquire(1)
    order: list[int] = []

    async def take(i: int) -> None:
        await bucket.acquire(1)
        order.append(i)

    await asyncio.gather(*(take(i) for i in range(3)))
    assert order == [0, 1, 2]
