//! The time source reservation expiry is measured against. Injected (rather
//! than read from `SystemTime` inline) so the expiry path is deterministic in
//! tests — [`ManualClock`] advances time explicitly instead of sleeping.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::types::UnixTsMillis;

/// A monotonic-enough source of wall-clock time in Unix milliseconds.
pub trait Clock: Send + Sync {
    /// The current time, as Unix milliseconds.
    fn now_ms(&self) -> UnixTsMillis;
}

/// The production clock: reads the system wall clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> UnixTsMillis {
        // A clock set before the epoch is nonsensical; treat it as the epoch.
        UnixTsMillis(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
                .unwrap_or(0),
        )
    }
}

/// A test clock whose time only moves when told to. Lets a test reserve, then
/// jump past `expires_at`, exercising the expiry path with no real delay.
#[derive(Debug, Default)]
pub struct ManualClock(AtomicU64);

impl ManualClock {
    /// A clock fixed at `start_ms`.
    pub fn new(start_ms: UnixTsMillis) -> Self {
        Self(AtomicU64::new(start_ms.get()))
    }

    /// Jump the clock forward by `delta_ms`.
    pub fn advance(&self, delta_ms: u64) {
        self.0.fetch_add(delta_ms, Ordering::SeqCst);
    }

    /// Set the clock to an absolute `ms`.
    pub fn set(&self, ms: UnixTsMillis) {
        self.0.store(ms.get(), Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> UnixTsMillis {
        UnixTsMillis(self.0.load(Ordering::SeqCst))
    }
}
