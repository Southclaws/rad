//! Explicit sources of nondeterminism used by the engine.
//!
//! Scheduling and storage completion order will remain separate concerns. This
//! boundary covers values that would otherwise be read from process-global
//! state and therefore could not be replayed by deterministic simulation.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Clock and generated-identifier effects used by catalog and data work.
///
/// Implementations must be safe to call concurrently. A deterministic
/// scheduler can provide a recorded implementation while production uses
/// [`SystemRuntime`].
pub trait RuntimeEffects: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
    fn new_uuid(&self) -> Uuid;

    /// Wall time since the Unix epoch. Durable recency fields use this value.
    fn unix_time(&self) -> Duration {
        Duration::from_micros(self.now().timestamp_micros().max(0) as u64)
    }

    /// Monotonic reading since an arbitrary fixed origin, for measuring
    /// elapsed intervals. Statistics timing must use this seam, never the
    /// process clock directly. The default returns zero so deterministic
    /// runtimes observe zero durations and replay identically.
    fn monotonic(&self) -> Duration {
        Duration::ZERO
    }
}

/// Production effects backed by the process clock and UUID implementation.
#[derive(Debug, Default)]
pub struct SystemRuntime;

impl RuntimeEffects for SystemRuntime {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn new_uuid(&self) -> Uuid {
        Uuid::new_v4()
    }

    fn monotonic(&self) -> Duration {
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        ORIGIN.get_or_init(Instant::now).elapsed()
    }
}
