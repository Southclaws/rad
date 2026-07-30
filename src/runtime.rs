//! Explicit sources of nondeterminism used by the engine.
//!
//! Scheduling and storage completion order will remain separate concerns. This
//! boundary covers values that would otherwise be read from process-global
//! state and therefore could not be replayed by deterministic simulation.

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
}
