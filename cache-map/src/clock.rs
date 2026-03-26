use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// A coarse monotonic clock that avoids per-read syscalls.
///
/// Instead of calling `Instant::now()` on every cache read (20-30ns on Windows),
/// callers read a cached timestamp via a single atomic load (~1ns).
/// The timestamp is updated by calling `tick()`, typically from a background
/// thread or periodically from the cleanup loop.
///
/// Resolution is as coarse as the `tick()` call frequency. For TTLs measured in
/// seconds, millisecond-level resolution is more than sufficient.
pub struct CoarseClock {
    /// Epoch from which all tick values are measured.
    epoch: Instant,
    /// Current time as milliseconds since `epoch`, atomically updated.
    now_ms: AtomicU64,
}

impl CoarseClock {
    /// Create a new clock anchored at the current instant.
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
            now_ms: AtomicU64::new(0),
        }
    }

    /// Update the cached timestamp.  Call this periodically (e.g. every 1-5ms).
    #[inline]
    pub fn tick(&self) {
        let ms = self.epoch.elapsed().as_millis() as u64;
        self.now_ms.store(ms, Ordering::Release);
    }

    /// Return the current cached time in milliseconds since epoch.
    /// This is a single atomic load — no syscall.
    #[inline]
    pub fn now_ms(&self) -> u64 {
        self.now_ms.load(Ordering::Acquire)
    }

    /// Convert a TTL duration into an absolute expiry timestamp (ms since epoch).
    #[inline]
    pub fn expire_at_ms(&self, ttl: std::time::Duration) -> u64 {
        // Use a real Instant::now() here — inserts are infrequent and need accuracy.
        let ms = self.epoch.elapsed().as_millis() as u64;
        ms + ttl.as_millis() as u64
    }

    /// Check whether a given expiry timestamp has passed.
    #[inline]
    pub fn is_expired(&self, expires_at_ms: u64) -> bool {
        self.now_ms() >= expires_at_ms
    }
}
