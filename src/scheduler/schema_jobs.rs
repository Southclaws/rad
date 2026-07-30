//! Tokio scheduling policy for durable schema work.

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep};

use crate::engine::catalog::Catalog;
use crate::engine::catalog::identity::{OwnerEpoch, ReclamationId, TransitionId};
use crate::engine::catalog::model::{ReclamationState, TransitionState};
use crate::engine::exec::schema_jobs::SchemaJob;
use crate::engine::exec::{Engine, Error, ErrorKind};

const DEFAULT_BATCH_SIZE: usize = 128;
const DEFAULT_BATCHES_PER_ROUND: usize = 16;
const DEFAULT_ITEM_BUDGET: usize = 2_048;
const DEFAULT_RETAIN_REVISIONS: u64 = 256;
const DEFAULT_HISTORY_BATCH_SIZE: usize = 128;
const DEFAULT_MAX_FAILURES: u32 = 8;

/// Process-local resource and retry policy for schema work.
#[derive(Clone, Debug)]
pub struct SchemaJobConfig {
    pub transition_batch_size: usize,
    pub reclamation_batch_size: usize,
    pub batches_per_round: usize,
    pub items_per_round: usize,
    pub yield_interval: Duration,
    pub idle_poll_interval: Duration,
    pub retry_backoff_min: Duration,
    pub retry_backoff_max: Duration,
    pub max_failures: u32,
    pub catalog_history_retain: u64,
    pub catalog_history_batch_size: usize,
}

impl Default for SchemaJobConfig {
    fn default() -> Self {
        Self {
            transition_batch_size: DEFAULT_BATCH_SIZE,
            reclamation_batch_size: DEFAULT_BATCH_SIZE,
            batches_per_round: DEFAULT_BATCHES_PER_ROUND,
            items_per_round: DEFAULT_ITEM_BUDGET,
            yield_interval: Duration::from_millis(1),
            idle_poll_interval: Duration::from_secs(1),
            retry_backoff_min: Duration::from_millis(5),
            retry_backoff_max: Duration::from_secs(1),
            max_failures: DEFAULT_MAX_FAILURES,
            catalog_history_retain: DEFAULT_RETAIN_REVISIONS,
            catalog_history_batch_size: DEFAULT_HISTORY_BATCH_SIZE,
        }
    }
}

impl SchemaJobConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        for (name, value) in [
            ("transition_batch_size", self.transition_batch_size),
            ("reclamation_batch_size", self.reclamation_batch_size),
            ("batches_per_round", self.batches_per_round),
            ("items_per_round", self.items_per_round),
            (
                "catalog_history_batch_size",
                self.catalog_history_batch_size,
            ),
        ] {
            if value == 0 {
                return Err(ConfigError::Zero(name));
            }
        }
        if self.max_failures == 0 {
            return Err(ConfigError::Zero("max_failures"));
        }
        for (name, value) in [
            ("yield_interval", self.yield_interval),
            ("idle_poll_interval", self.idle_poll_interval),
            ("retry_backoff_min", self.retry_backoff_min),
            ("retry_backoff_max", self.retry_backoff_max),
        ] {
            if value.is_zero() {
                return Err(ConfigError::Zero(name));
            }
        }
        if self.retry_backoff_max < self.retry_backoff_min {
            return Err(ConfigError::BackoffOrder);
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("schema scheduler setting {0} must be greater than zero")]
    Zero(&'static str),
    #[error("schema scheduler retry_backoff_max must not be less than retry_backoff_min")]
    BackoffOrder,
}

/// A semantic scheduler boundary exposed to deterministic test drivers.
#[derive(Clone, Debug)]
pub enum SchemaJobEvent {
    Started,
    Discovered {
        jobs: Vec<SchemaJob>,
    },
    BeforeJob {
        job: SchemaJob,
    },
    AfterJob {
        job: SchemaJob,
        items: usize,
    },
    RetryDeferred {
        job: SchemaJob,
        failures: u32,
        delay: Duration,
        cause: String,
    },
    Quarantined {
        job: SchemaJob,
        failures: u32,
        cause: String,
    },
    DiscoveryFailed {
        cause: String,
        delay: Duration,
    },
    Idle,
    Stopping,
    Stopped,
}

/// Async boundary hook for Turmoil and other deterministic schedule drivers.
///
/// Hooks run only between bounded kernel calls. They may suspend a worker to
/// choose a schedule, but must eventually return so graceful shutdown can
/// finish.
#[async_trait]
pub trait SchemaJobHook: Send + Sync {
    async fn reach(&self, event: SchemaJobEvent);
}

#[derive(Debug, Default)]
pub struct NoopSchemaJobHook;

#[async_trait]
impl SchemaJobHook for NoopSchemaJobHook {
    async fn reach(&self, _event: SchemaJobEvent) {}
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SchemaJobStats {
    pub rounds: u64,
    pub batches: u64,
    pub items: u64,
    pub retries: u64,
    pub quarantined: u64,
}

#[derive(Default)]
struct AtomicStats {
    rounds: AtomicU64,
    batches: AtomicU64,
    items: AtomicU64,
    retries: AtomicU64,
    quarantined: AtomicU64,
}

impl AtomicStats {
    fn load(&self) -> SchemaJobStats {
        SchemaJobStats {
            rounds: self.rounds.load(Ordering::Relaxed),
            batches: self.batches.load(Ordering::Relaxed),
            items: self.items.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            quarantined: self.quarantined.load(Ordering::Relaxed),
        }
    }
}

struct Shared {
    wake: Notify,
    stop: Notify,
    wake_epoch: AtomicU64,
    stopping: AtomicBool,
    stats: AtomicStats,
    last_error: RwLock<Option<String>>,
}

impl Shared {
    fn wake(&self) {
        self.wake_epoch.fetch_add(1, Ordering::Release);
        self.wake.notify_one();
    }
}

/// A production Tokio task that advances durable schema jobs fairly.
///
/// Dropping the handle requests shutdown. Call [`Self::shutdown`] when the
/// process must wait for the current bounded transaction to finish.
pub struct SchemaJobRunner {
    shared: Arc<Shared>,
    task: AsyncMutex<Option<JoinHandle<()>>>,
}

impl SchemaJobRunner {
    pub fn start(engine: Arc<Engine>, config: SchemaJobConfig) -> Result<Self, ConfigError> {
        Self::start_with_hook(engine, config, Arc::new(NoopSchemaJobHook))
    }

    pub fn start_with_hook(
        engine: Arc<Engine>,
        config: SchemaJobConfig,
        hook: Arc<dyn SchemaJobHook>,
    ) -> Result<Self, ConfigError> {
        config.validate()?;
        let shared = Arc::new(Shared {
            wake: Notify::new(),
            stop: Notify::new(),
            wake_epoch: AtomicU64::new(1),
            stopping: AtomicBool::new(false),
            stats: AtomicStats::default(),
            last_error: RwLock::new(None),
        });
        let engine_wake = Arc::downgrade(&shared);
        engine.on_catalog_change(move || wake_weak(&engine_wake));
        let worker_shared = shared.clone();
        let task = tokio::spawn(async move {
            Worker::new(engine, config, worker_shared, hook).run().await;
        });
        Ok(Self {
            shared,
            task: AsyncMutex::new(Some(task)),
        })
    }

    /// Register catalog notifications as low-latency hints. Startup discovery
    /// and idle polling remain authoritative, so missed or coalesced hints are
    /// harmless.
    pub fn observe_catalog(&self, catalog: &Catalog) {
        let shared = Arc::downgrade(&self.shared);
        catalog.on_change(move || wake_weak(&shared));
    }

    /// Hint that newly committed catalog work may be ready.
    pub fn wake(&self) {
        self.shared.wake();
    }

    pub fn stats(&self) -> SchemaJobStats {
        self.shared.stats.load()
    }

    pub fn last_error(&self) -> Option<String> {
        self.shared
            .last_error
            .read()
            .expect("schema scheduler error lock poisoned")
            .clone()
    }

    /// Request shutdown and wait for the current bounded kernel call. The
    /// kernel future is never dropped merely because shutdown was requested.
    pub async fn shutdown(&self) -> Result<(), tokio::task::JoinError> {
        self.request_stop();
        if let Some(task) = self.task.lock().await.take() {
            task.await?;
        }
        Ok(())
    }

    fn request_stop(&self) {
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.stop.notify_one();
        self.shared.wake.notify_one();
    }
}

impl Drop for SchemaJobRunner {
    fn drop(&mut self) {
        self.request_stop();
    }
}

fn wake_weak(shared: &Weak<Shared>) {
    if let Some(shared) = shared.upgrade() {
        shared.wake();
    }
}

struct RetryState {
    backoff_step: u32,
    failures: u32,
    next: Instant,
}

struct Worker {
    engine: Arc<Engine>,
    config: SchemaJobConfig,
    shared: Arc<Shared>,
    hook: Arc<dyn SchemaJobHook>,
    next: usize,
    seen_wake_epoch: u64,
    transition_owners: BTreeMap<TransitionId, OwnerEpoch>,
    reclamation_owners: BTreeMap<ReclamationId, OwnerEpoch>,
    retries: BTreeMap<SchemaJob, RetryState>,
    quarantined: BTreeSet<SchemaJob>,
}

impl Worker {
    fn new(
        engine: Arc<Engine>,
        config: SchemaJobConfig,
        shared: Arc<Shared>,
        hook: Arc<dyn SchemaJobHook>,
    ) -> Self {
        Self {
            engine,
            config,
            seen_wake_epoch: shared.wake_epoch.load(Ordering::Acquire),
            shared,
            hook,
            next: 0,
            transition_owners: BTreeMap::new(),
            reclamation_owners: BTreeMap::new(),
            retries: BTreeMap::new(),
            quarantined: BTreeSet::new(),
        }
    }

    async fn run(mut self) {
        self.hook.reach(SchemaJobEvent::Started).await;
        loop {
            if self.is_stopping() {
                break;
            }
            self.accept_wake();
            let jobs = match self
                .engine
                .discover_schema_jobs(self.config.catalog_history_retain)
                .await
            {
                Ok(jobs) => jobs,
                Err(error) => {
                    let delay = self.config.retry_backoff_max;
                    self.set_error(&error);
                    self.hook
                        .reach(SchemaJobEvent::DiscoveryFailed {
                            cause: error.to_string(),
                            delay,
                        })
                        .await;
                    if self.wait(delay).await == WaitOutcome::Stopping {
                        break;
                    }
                    continue;
                }
            };
            self.hook
                .reach(SchemaJobEvent::Discovered { jobs: jobs.clone() })
                .await;
            let runnable = jobs
                .into_iter()
                .filter(|job| !self.quarantined.contains(job))
                .collect::<Vec<_>>();
            if runnable.is_empty() {
                if self.quarantined.is_empty() {
                    self.clear_error();
                }
                self.hook.reach(SchemaJobEvent::Idle).await;
                match self.wait(self.config.idle_poll_interval).await {
                    WaitOutcome::Stopping => break,
                    WaitOutcome::Woken | WaitOutcome::Elapsed => {
                        self.reset_deferred_work();
                        continue;
                    }
                }
            }
            let progressed = self.run_round(&runnable).await;
            if self.is_stopping() {
                break;
            }
            let delay = if progressed {
                self.config.yield_interval
            } else {
                self.retry_delay(self.config.yield_interval)
            };
            if self.wait(delay).await == WaitOutcome::Stopping {
                break;
            }
        }
        self.hook.reach(SchemaJobEvent::Stopping).await;
        self.hook.reach(SchemaJobEvent::Stopped).await;
    }

    async fn run_round(&mut self, jobs: &[SchemaJob]) -> bool {
        self.shared.stats.rounds.fetch_add(1, Ordering::Relaxed);
        if self.next >= jobs.len() {
            self.next = 0;
        }
        let batch_budget = self.config.batches_per_round.min(jobs.len());
        let mut item_budget = self.config.items_per_round;
        let mut visited = 0;
        let mut progressed = false;
        for offset in 0..batch_budget {
            if item_budget == 0 || self.is_stopping() {
                break;
            }
            visited += 1;
            let position = (self.next + offset) % jobs.len();
            let job = jobs[position].clone();
            if self.retry_pending(&job) {
                continue;
            }
            self.hook
                .reach(SchemaJobEvent::BeforeJob { job: job.clone() })
                .await;
            let outcome = self.advance(&job, item_budget).await;
            match outcome {
                Ok(items) => {
                    self.retries.remove(&job);
                    self.clear_error();
                    if items > 0 {
                        progressed = true;
                        item_budget = item_budget.saturating_sub(items);
                        self.shared.stats.batches.fetch_add(1, Ordering::Relaxed);
                        self.shared
                            .stats
                            .items
                            .fetch_add(items as u64, Ordering::Relaxed);
                    }
                    self.hook
                        .reach(SchemaJobEvent::AfterJob { job, items })
                        .await;
                }
                Err(error) => self.handle_error(job, error).await,
            }
        }
        self.next = (self.next + visited) % jobs.len();
        progressed
    }

    async fn advance(&mut self, job: &SchemaJob, item_budget: usize) -> Result<usize, Error> {
        match job {
            SchemaJob::Activation(id) => {
                let transition = self.engine.activate_waiting_schema_transition(id).await?;
                if transition.state == TransitionState::Waiting {
                    return Err(ErrorProxy::waiting(id).into_error());
                }
                Ok(1)
            }
            SchemaJob::Transition(id) => {
                let owner = match self.transition_owners.get(id).copied() {
                    Some(owner) => owner,
                    None => {
                        let owner = self.engine.claim_schema_transition(id).await?;
                        self.transition_owners.insert(id.clone(), owner);
                        owner
                    }
                };
                let batch_size = self.config.transition_batch_size.min(item_budget).max(1);
                let step = self
                    .engine
                    .step_schema_transition(id, owner, batch_size)
                    .await?;
                if step.transition.state.is_terminal() {
                    self.transition_owners.remove(id);
                }
                Ok(step.items.max(1))
            }
            SchemaJob::Reclamation(id) => {
                let owner = match self.reclamation_owners.get(id).copied() {
                    Some(owner) => owner,
                    None => {
                        let Some(owner) = self.engine.claim_reclamation(id).await? else {
                            return Ok(0);
                        };
                        self.reclamation_owners.insert(id.clone(), owner);
                        owner
                    }
                };
                let batch_size = self.config.reclamation_batch_size.min(item_budget).max(1);
                let (reclamation, items) =
                    self.engine.step_reclamation(id, owner, batch_size).await?;
                if reclamation.state == ReclamationState::Reclaimed {
                    self.reclamation_owners.remove(id);
                }
                Ok(items.max(1))
            }
            SchemaJob::TransitionCompaction(id) => {
                Ok(usize::from(self.engine.compact_transition_step(id).await?))
            }
            SchemaJob::CatalogHistory => {
                let batch_size = self
                    .config
                    .catalog_history_batch_size
                    .min(item_budget)
                    .max(1);
                let (deleted, _) = self
                    .engine
                    .compact_catalog_history_step(self.config.catalog_history_retain, batch_size)
                    .await?;
                Ok(deleted)
            }
        }
    }

    async fn handle_error(&mut self, job: SchemaJob, error: Error) {
        self.set_error(&error);
        if matches!(job, SchemaJob::Transition(_)) && !is_contention(&error) {
            self.record_transition_error(&job, &error).await;
        }
        if is_contention(&error) {
            self.forget_owner(&job);
            self.defer(job, 0, error.to_string()).await;
            return;
        }
        let failures = self
            .retries
            .get(&job)
            .map_or(1, |retry| retry.failures.saturating_add(1));
        if failures >= self.config.max_failures {
            if let SchemaJob::Reclamation(id) = &job
                && let Some(owner) = self.reclamation_owners.get(id).copied()
            {
                let _ = self
                    .engine
                    .fail_reclamation(id, owner, error.to_string())
                    .await;
            }
            self.forget_owner(&job);
            self.retries.remove(&job);
            self.quarantined.insert(job.clone());
            self.shared
                .stats
                .quarantined
                .fetch_add(1, Ordering::Relaxed);
            self.hook
                .reach(SchemaJobEvent::Quarantined {
                    job,
                    failures,
                    cause: error.to_string(),
                })
                .await;
            return;
        }
        self.defer(job, failures, error.to_string()).await;
    }

    async fn record_transition_error(&self, job: &SchemaJob, error: &Error) {
        let SchemaJob::Transition(id) = job else {
            return;
        };
        let Some(owner) = self.transition_owners.get(id).copied() else {
            return;
        };
        let _ = self
            .engine
            .record_schema_transition_error(id, owner, error.to_string())
            .await;
    }

    async fn defer(&mut self, job: SchemaJob, failures: u32, cause: String) {
        let previous_step = self.retries.get(&job).map_or(0, |retry| retry.backoff_step);
        let backoff_step = previous_step
            .saturating_add(1)
            .min(self.config.max_failures);
        let delay = exponential_backoff(
            self.config.retry_backoff_min,
            self.config.retry_backoff_max,
            backoff_step,
        );
        self.retries.insert(
            job.clone(),
            RetryState {
                backoff_step,
                failures,
                next: Instant::now() + delay,
            },
        );
        self.shared.stats.retries.fetch_add(1, Ordering::Relaxed);
        self.hook
            .reach(SchemaJobEvent::RetryDeferred {
                job,
                failures,
                delay,
                cause,
            })
            .await;
    }

    fn retry_pending(&self, job: &SchemaJob) -> bool {
        self.retries
            .get(job)
            .is_some_and(|retry| retry.next > Instant::now())
    }

    fn retry_delay(&self, fallback: Duration) -> Duration {
        let now = Instant::now();
        self.retries
            .values()
            .filter_map(|retry| retry.next.checked_duration_since(now))
            .min()
            .map_or(fallback, |delay| delay.max(fallback))
    }

    fn forget_owner(&mut self, job: &SchemaJob) {
        match job {
            SchemaJob::Transition(id) => {
                self.transition_owners.remove(id);
            }
            SchemaJob::Reclamation(id) => {
                self.reclamation_owners.remove(id);
            }
            SchemaJob::Activation(_)
            | SchemaJob::TransitionCompaction(_)
            | SchemaJob::CatalogHistory => {}
        }
    }

    fn accept_wake(&mut self) {
        let epoch = self.shared.wake_epoch.load(Ordering::Acquire);
        if epoch != self.seen_wake_epoch {
            self.seen_wake_epoch = epoch;
            self.reset_deferred_work();
        }
    }

    fn reset_deferred_work(&mut self) {
        self.retries.clear();
        self.quarantined.clear();
    }

    fn set_error(&self, error: &Error) {
        *self
            .shared
            .last_error
            .write()
            .expect("schema scheduler error lock poisoned") = Some(error.to_string());
    }

    fn clear_error(&self) {
        *self
            .shared
            .last_error
            .write()
            .expect("schema scheduler error lock poisoned") = None;
    }

    fn is_stopping(&self) -> bool {
        self.shared.stopping.load(Ordering::Acquire)
    }

    async fn wait(&mut self, duration: Duration) -> WaitOutcome {
        if self.is_stopping() {
            return WaitOutcome::Stopping;
        }
        tokio::select! {
            _ = self.shared.stop.notified() => WaitOutcome::Stopping,
            _ = self.shared.wake.notified() => {
                self.accept_wake();
                WaitOutcome::Woken
            },
            _ = sleep(duration) => WaitOutcome::Elapsed,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WaitOutcome {
    Woken,
    Elapsed,
    Stopping,
}

fn is_contention(error: &Error) -> bool {
    error.is_conflict() || error.kind() == ErrorKind::CommitOutcomeUnknown
}

fn exponential_backoff(minimum: Duration, maximum: Duration, step: u32) -> Duration {
    let shifts = step.saturating_sub(1).min(31);
    minimum.saturating_mul(1_u32 << shifts).min(maximum)
}

// Waiting on a prerequisite is expected scheduler contention, not a durable
// transition failure. Engine errors intentionally cannot be constructed by
// orchestration code, so this tiny adapter keeps that classification local.
struct ErrorProxy(String);

impl ErrorProxy {
    fn waiting(id: &TransitionId) -> Self {
        Self(format!(
            "schema transition {id} is waiting for prerequisites"
        ))
    }

    fn into_error(self) -> Error {
        // Feed the condition through the catalog error conversion so the
        // scheduler sees the same Conflict class as storage contention.
        crate::engine::catalog::Error::message(crate::engine::catalog::ErrorKind::Conflict, self.0)
            .into()
    }
}
