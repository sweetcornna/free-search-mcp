import asyncio
import time
from collections import defaultdict


class TokenBucket:
    def __init__(self, rate_per_minute: int, burst: int | None = None):
        self.rate = rate_per_minute / 60.0
        self.capacity = burst or max(rate_per_minute, 1)
        self.tokens = float(self.capacity)
        self.last = time.monotonic()
        self._lock = asyncio.Lock()

    async def acquire(self, n: int = 1) -> None:
        async with self._lock:
            while True:
                now = time.monotonic()
                self.tokens = min(self.capacity, self.tokens + (now - self.last) * self.rate)
                self.last = now
                if self.tokens >= n:
                    self.tokens -= n
                    return
                wait = (n - self.tokens) / self.rate
                await asyncio.sleep(wait)


class RateLimiter:
    def __init__(self, default_rpm: int):
        self.default_rpm = default_rpm
        self._buckets: dict[str, TokenBucket] = defaultdict(lambda: TokenBucket(self.default_rpm))

    def configure(self, key: str, rpm: int) -> None:
        self._buckets[key] = TokenBucket(rpm)

    async def acquire(self, key: str) -> None:
        await self._buckets[key].acquire()
