import asyncio
import time
from collections import defaultdict


class TokenBucket:
    def __init__(self, rate_per_minute: int, burst: int | None = None):
        self.rate = rate_per_minute / 60.0
        self.capacity = burst or max(rate_per_minute, 1)
        self.tokens = float(self.capacity)
        self.last = time.monotonic()

    async def acquire(self, n: int = 1, max_wait: float | None = None) -> bool:
        """Take ``n`` tokens, waiting for them to refill.

        Returns True once the tokens are taken. With ``max_wait`` set, returns
        False instead of waiting longer than that — for sources whose rate
        limit is slow enough that queueing would stall the caller. The tokens
        are NOT consumed on a False return.

        A caller that has to wait reserves its tokens first (the bucket goes
        into debt) and then sleeps, so callers are served in arrival order and
        nobody holds anything while asleep. The bucket used to sleep inside a
        lock, and then even a caller that would not wait at all (`max_wait=0`,
        the agent lane) queued behind a sleeper for the full refill time.
        """
        # Defense-in-depth: a non-positive rate means "unlimited" rather than
        # dividing by zero (or sleeping a negative/NaN duration) below. Config
        # already rejects rate_per_minute<=0 via Field(gt=0), but a TokenBucket
        # constructed directly must not blow up.
        if self.rate <= 0:
            return True
        # No await between reading and writing the bucket, so on one event
        # loop this needs no lock.
        now = time.monotonic()
        self.tokens = min(self.capacity, self.tokens + (now - self.last) * self.rate)
        self.last = now
        wait = 0.0 if self.tokens >= n else (n - self.tokens) / self.rate
        if max_wait is not None and wait > max_wait:
            return False
        self.tokens -= n
        if wait > 0:
            try:
                await asyncio.sleep(wait)
            except asyncio.CancelledError:
                # Nothing was sent: hand the reservation back, or every
                # cancelled waiter (a rescue race's loser, an engine past the
                # search deadline) would leave the bucket in debt.
                self.tokens += n
                raise
        return True


class RateLimiter:
    def __init__(self, default_rpm: int):
        self.default_rpm = default_rpm
        self._buckets: dict[str, TokenBucket] = defaultdict(lambda: TokenBucket(self.default_rpm))

    def configure(self, key: str, rpm: int, burst: int | None = None) -> None:
        self._buckets[key] = TokenBucket(rpm, burst)

    async def acquire(self, key: str, max_wait: float | None = None) -> bool:
        return await self._buckets[key].acquire(max_wait=max_wait)
