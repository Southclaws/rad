//! Process health observed by orchestrator probes.
//!
//! The runtime writes lifecycle transitions and storage observations here; the
//! `/startupz`, `/readyz`, and `/livez` endpoints only read them. Keeping the
//! observations cached is the point: an orchestrator probes far more often than
//! an object store should be asked whether it is reachable, so a background task
//! refreshes the observation and readiness reports its freshness.
//!
//! Each probe answers a different question and so has its own set of reasons.
//! Nothing here is a general status API: an orchestrator decides from the HTTP
//! status alone, and the reason exists for whoever has to explain the decision.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub const STORAGE_OBSERVATION_INTERVAL: Duration = Duration::from_secs(1);

/// How long a successful storage observation keeps readiness. The window
/// tolerates several missed refreshes so a single slow object-store round trip
/// does not withdraw traffic, while a wedged observer still expires.
pub const STORAGE_FRESHNESS: Duration = Duration::from_secs(15);

/// Whether initialization finished. Startup is a latch: once the database is
/// published it stays published, and later faults are readiness or liveness
/// questions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Startup {
    Starting,
    Started,
}

impl Startup {
    pub const fn passed(self) -> bool {
        matches!(self, Self::Started)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Started => "started",
        }
    }
}

/// The cause of a held startup. A preflight sets this, not the storage open:
/// SlateDB retries an unavailable object store without limit, so the open
/// does not return an error to classify.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartupHold {
    /// Storage answers; initialization is simply still running.
    Opening,
    /// The configured bucket does not exist.
    BucketMissing,
    /// The object store rejects the configured credentials.
    Unauthorized,
    /// The object store cannot be reached at all.
    Unreachable,
}

impl StartupHold {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Opening => "starting",
            Self::BucketMissing => "storage_bucket_missing",
            Self::Unauthorized => "storage_unauthorized",
            Self::Unreachable => "storage_unreachable",
        }
    }

    const fn code(self) -> u8 {
        match self {
            Self::Opening => 0,
            Self::BucketMissing => 1,
            Self::Unauthorized => 2,
            Self::Unreachable => 3,
        }
    }

    const fn from_code(code: u8) -> Self {
        match code {
            1 => Self::BucketMissing,
            2 => Self::Unauthorized,
            3 => Self::Unreachable,
            _ => Self::Opening,
        }
    }
}

/// Whether traffic belongs here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Readiness {
    Starting,
    Serving,
    Draining,
    /// Another writer took ownership of the storage location.
    Fenced,
    /// The most recent storage observation failed.
    StorageUnavailable,
    /// No storage observation succeeded inside the freshness window.
    StorageStale,
}

impl Readiness {
    pub const fn passed(self) -> bool {
        matches!(self, Self::Serving)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Serving => "serving",
            Self::Draining => "draining",
            Self::Fenced => "fenced",
            Self::StorageUnavailable => "storage_unavailable",
            Self::StorageStale => "storage_stale",
        }
    }
}

/// Whether restarting the process could help. Storage is deliberately absent:
/// a process restarted for an object-store outage comes back to the same
/// outage, so storage faults withdraw traffic through readiness instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Liveness {
    Live,
    Draining,
    /// A critical background task stopped without being asked to.
    TaskFailed,
}

impl Liveness {
    pub const fn passed(self) -> bool {
        !matches!(self, Self::TaskFailed)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Draining => "draining",
            Self::TaskFailed => "task_failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Starting,
    Serving,
    Draining,
}

impl Phase {
    const fn code(self) -> u8 {
        match self {
            Self::Starting => 0,
            Self::Serving => 1,
            Self::Draining => 2,
        }
    }

    const fn from_code(code: u8) -> Self {
        match code {
            0 => Self::Starting,
            1 => Self::Serving,
            _ => Self::Draining,
        }
    }
}

/// Sentinel for "storage has never been observed", distinct from an observation
/// that succeeded at process start.
const UNOBSERVED: u64 = u64::MAX;

/// Shared, lock-free process health.
#[derive(Debug)]
pub struct Health {
    freshness: Duration,
    started: Instant,
    phase: AtomicU8,
    fenced: AtomicBool,
    task_failed: AtomicBool,
    storage_failing: AtomicBool,
    storage_observed_at: AtomicU64,
    startup_hold: AtomicU8,
}

impl Health {
    pub fn starting(freshness: Duration) -> Arc<Self> {
        Arc::new(Self {
            freshness,
            started: Instant::now(),
            phase: AtomicU8::new(Phase::Starting.code()),
            fenced: AtomicBool::new(false),
            task_failed: AtomicBool::new(false),
            storage_failing: AtomicBool::new(false),
            storage_observed_at: AtomicU64::new(UNOBSERVED),
            startup_hold: AtomicU8::new(StartupHold::Opening.code()),
        })
    }

    /// Health for a router with no separate runtime to observe it, such as an
    /// embedded or in-process server. Storage stays unobserved rather than
    /// fresh, which readiness treats as nothing having reported a fault.
    pub fn serving() -> Arc<Self> {
        let health = Self::starting(STORAGE_FRESHNESS);
        health.serve();
        health
    }

    /// Publish the database: storage is open, the catalog's identity and mode
    /// are validated, and the required background workers are running.
    pub fn serve(&self) {
        self.phase.store(Phase::Serving.code(), Ordering::Release);
    }

    pub fn drain(&self) {
        self.phase.store(Phase::Draining.code(), Ordering::Release);
    }

    pub fn observe_storage(&self) -> bool {
        let elapsed = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(UNOBSERVED - 1);
        self.storage_observed_at.store(elapsed, Ordering::Release);
        self.storage_failing.swap(false, Ordering::AcqRel)
    }

    /// Returns whether this changed the observed state, so a caller can log the
    /// transition instead of every failed retry.
    pub fn observe_storage_unavailable(&self) -> bool {
        !self.storage_failing.swap(true, Ordering::AcqRel)
    }

    pub fn observe_fenced(&self) {
        self.fenced.store(true, Ordering::Release);
    }

    pub fn observe_task_failure(&self) {
        self.task_failed.store(true, Ordering::Release);
    }

    pub fn startup(&self) -> Startup {
        match self.phase() {
            Phase::Starting => Startup::Starting,
            Phase::Serving | Phase::Draining => Startup::Started,
        }
    }

    /// Record the preflight's diagnosis of why startup is held. Returns
    /// whether the diagnosis changed, so a caller can log transitions.
    pub fn observe_startup_hold(&self, hold: StartupHold) -> bool {
        self.startup_hold.swap(hold.code(), Ordering::AcqRel) != hold.code()
    }

    pub fn startup_hold(&self) -> StartupHold {
        StartupHold::from_code(self.startup_hold.load(Ordering::Acquire))
    }

    /// Fencing outranks the lifecycle phase so a writer that lost its storage
    /// location says so rather than reporting an ordinary drain.
    pub fn ready(&self) -> Readiness {
        if self.fenced.load(Ordering::Acquire) {
            return Readiness::Fenced;
        }
        match self.phase() {
            Phase::Starting => Readiness::Starting,
            Phase::Draining => Readiness::Draining,
            Phase::Serving => self.storage_readiness(),
        }
    }

    pub fn live(&self) -> Liveness {
        if self.task_failed.load(Ordering::Acquire) {
            return Liveness::TaskFailed;
        }
        match self.phase() {
            Phase::Starting | Phase::Serving => Liveness::Live,
            Phase::Draining => Liveness::Draining,
        }
    }

    fn storage_readiness(&self) -> Readiness {
        if self.storage_failing.load(Ordering::Acquire) {
            return Readiness::StorageUnavailable;
        }
        let observed = self.storage_observed_at.load(Ordering::Acquire);
        if observed == UNOBSERVED {
            return Readiness::Serving;
        }
        let deadline = u128::from(observed) + self.freshness.as_millis();
        if self.started.elapsed().as_millis() > deadline {
            Readiness::StorageStale
        } else {
            Readiness::Serving
        }
    }

    fn phase(&self) -> Phase {
        Phase::from_code(self.phase.load(Ordering::Acquire))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn health() -> Arc<Health> {
        Health::starting(Duration::from_millis(50))
    }

    #[test]
    fn a_starting_process_is_live_but_neither_started_nor_ready() {
        let health = health();

        assert_eq!(health.startup(), Startup::Starting);
        assert_eq!(health.ready(), Readiness::Starting);
        assert_eq!(health.live(), Liveness::Live);
        assert!(!health.startup().passed());
        assert!(!health.ready().passed());
        assert!(health.live().passed());
    }

    #[test]
    fn startup_hold_diagnosis_reports_transitions_once() {
        let health = health();
        assert_eq!(health.startup_hold(), StartupHold::Opening);

        assert!(health.observe_startup_hold(StartupHold::Unreachable));
        assert!(!health.observe_startup_hold(StartupHold::Unreachable));
        assert_eq!(health.startup_hold(), StartupHold::Unreachable);
        assert_eq!(health.startup_hold().as_str(), "storage_unreachable");

        assert!(health.observe_startup_hold(StartupHold::BucketMissing));
        assert_eq!(health.startup_hold().as_str(), "storage_bucket_missing");
        assert!(health.observe_startup_hold(StartupHold::Unauthorized));
        assert_eq!(health.startup_hold().as_str(), "storage_unauthorized");
        assert!(health.observe_startup_hold(StartupHold::Opening));
        assert_eq!(health.startup_hold().as_str(), "starting");
    }

    #[test]
    fn serving_passes_every_probe_before_storage_is_observed() {
        let health = health();
        health.serve();

        assert_eq!(health.startup(), Startup::Started);
        assert_eq!(health.ready(), Readiness::Serving);
        assert_eq!(health.live(), Liveness::Live);
    }

    #[test]
    fn draining_withdraws_readiness_while_startup_and_liveness_hold() {
        let health = health();
        health.serve();
        health.drain();

        assert!(health.startup().passed());
        assert_eq!(health.ready(), Readiness::Draining);
        assert!(!health.ready().passed());
        assert_eq!(health.live(), Liveness::Draining);
        assert!(health.live().passed());
    }

    #[test]
    fn a_fenced_writer_is_unready_immediately_and_still_live() {
        let health = health();
        health.serve();
        health.observe_storage();
        health.observe_fenced();

        assert_eq!(health.ready(), Readiness::Fenced);
        assert!(health.live().passed());

        health.drain();
        assert_eq!(health.ready(), Readiness::Fenced);
    }

    #[test]
    fn a_recoverable_storage_fault_withdraws_traffic_without_failing_liveness() {
        let health = health();
        health.serve();
        health.observe_storage();

        assert!(health.observe_storage_unavailable());
        assert!(!health.observe_storage_unavailable());
        assert_eq!(health.ready(), Readiness::StorageUnavailable);
        assert!(health.live().passed());

        health.observe_storage();
        assert!(health.ready().passed());
    }

    #[test]
    fn an_observation_older_than_the_freshness_window_withdraws_traffic() {
        let health = health();
        health.serve();
        health.observe_storage();
        std::thread::sleep(Duration::from_millis(120));

        assert_eq!(health.ready(), Readiness::StorageStale);
        assert!(health.live().passed());
    }

    #[test]
    fn a_lost_critical_task_fails_liveness() {
        let health = health();
        health.serve();
        health.observe_storage();
        health.observe_task_failure();

        assert_eq!(health.live(), Liveness::TaskFailed);
        assert!(!health.live().passed());
    }
}
