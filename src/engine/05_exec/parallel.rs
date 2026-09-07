use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ExecutionScheduleEvent {
    Prepared {
        operator: &'static str,
        sequence: u64,
        rows: usize,
        width: usize,
    },
    Completed {
        operator: &'static str,
        sequence: u64,
    },
    Published {
        operator: &'static str,
        sequence: u64,
        rows: usize,
    },
}

#[async_trait::async_trait]
pub(super) trait ExecutionScheduleHook: Send + Sync {
    async fn reach(&self, event: ExecutionScheduleEvent);
}

#[derive(Debug, Default)]
struct NoopExecutionScheduleHook;

#[async_trait::async_trait]
impl ExecutionScheduleHook for NoopExecutionScheduleHook {
    async fn reach(&self, _event: ExecutionScheduleEvent) {}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WorkRequest {
    pub operator: &'static str,
    pub morsels: usize,
}

pub(super) trait ParallelismPolicy: Send + Sync {
    fn width(&self, request: WorkRequest, cpu_limit: usize, active: usize) -> usize;
}

#[derive(Debug, Default)]
struct AdaptivePolicy;

impl ParallelismPolicy for AdaptivePolicy {
    fn width(&self, request: WorkRequest, cpu_limit: usize, active: usize) -> usize {
        let fair_width = cpu_limit / active.max(1);
        fair_width.max(1).min(request.morsels.max(1))
    }
}

pub(super) struct ExecutionScheduler {
    cpu_limit: usize,
    active: AtomicUsize,
    helpers: Arc<Semaphore>,
    policy: Arc<dyn ParallelismPolicy>,
    hook: Arc<dyn ExecutionScheduleHook>,
}

impl ExecutionScheduler {
    pub(super) fn adaptive(cpu_limit: usize) -> Arc<Self> {
        Self::with_policy(
            cpu_limit,
            Arc::new(AdaptivePolicy),
            Arc::new(NoopExecutionScheduleHook),
        )
    }

    fn with_policy(
        cpu_limit: usize,
        policy: Arc<dyn ParallelismPolicy>,
        hook: Arc<dyn ExecutionScheduleHook>,
    ) -> Arc<Self> {
        let cpu_limit = cpu_limit.max(1);
        Arc::new(Self {
            cpu_limit,
            active: AtomicUsize::new(0),
            helpers: Arc::new(Semaphore::new(cpu_limit.saturating_sub(1))),
            policy,
            hook,
        })
    }

    pub(super) fn enter(self: &Arc<Self>) -> ExecutionGrant {
        self.active.fetch_add(1, Ordering::AcqRel);
        ExecutionGrant {
            inner: Arc::new(ExecutionGrantInner {
                scheduler: self.clone(),
            }),
        }
    }

    pub(super) fn serial() -> ExecutionGrant {
        Self::adaptive(1).enter()
    }

    #[cfg(test)]
    pub(super) fn fixed(cpu_limit: usize, widths: impl IntoIterator<Item = usize>) -> Arc<Self> {
        Self::with_policy(
            cpu_limit,
            Arc::new(ScriptedPolicy::new(widths)),
            Arc::new(NoopExecutionScheduleHook),
        )
    }

    #[cfg(test)]
    pub(super) fn fixed_with_hook(
        cpu_limit: usize,
        widths: impl IntoIterator<Item = usize>,
        hook: Arc<dyn ExecutionScheduleHook>,
    ) -> Arc<Self> {
        Self::with_policy(cpu_limit, Arc::new(ScriptedPolicy::new(widths)), hook)
    }
}

struct ExecutionGrantInner {
    scheduler: Arc<ExecutionScheduler>,
}

impl Drop for ExecutionGrantInner {
    fn drop(&mut self) {
        self.scheduler.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone)]
pub(super) struct ExecutionGrant {
    inner: Arc<ExecutionGrantInner>,
}

impl ExecutionGrant {
    pub(super) fn serial() -> Self {
        ExecutionScheduler::serial()
    }

    pub(super) fn try_lease(&self, request: WorkRequest) -> ParallelLease {
        let scheduler = &self.inner.scheduler;
        let active = scheduler.active.load(Ordering::Acquire).max(1);
        let requested = scheduler
            .policy
            .width(request, scheduler.cpu_limit, active)
            .max(1)
            .min(request.morsels.max(1))
            .min(scheduler.cpu_limit);
        let mut helpers = Vec::with_capacity(requested.saturating_sub(1));
        for _ in 1..requested {
            let Ok(permit) = scheduler.helpers.clone().try_acquire_owned() else {
                break;
            };
            helpers.push(permit);
        }
        ParallelLease { helpers }
    }

    pub(super) async fn reach(&self, event: ExecutionScheduleEvent) {
        self.inner.scheduler.hook.reach(event).await;
    }
}

pub(super) struct ParallelLease {
    helpers: Vec<OwnedSemaphorePermit>,
}

impl ParallelLease {
    pub(super) fn width(&self) -> usize {
        self.helpers.len() + 1
    }

    pub(super) fn into_helpers(self) -> Vec<OwnedSemaphorePermit> {
        self.helpers
    }
}

#[cfg(test)]
struct ScriptedPolicy {
    widths: Mutex<std::collections::VecDeque<usize>>,
}

#[cfg(test)]
impl ScriptedPolicy {
    fn new(widths: impl IntoIterator<Item = usize>) -> Self {
        Self {
            widths: Mutex::new(widths.into_iter().collect()),
        }
    }
}

#[cfg(test)]
impl ParallelismPolicy for ScriptedPolicy {
    fn width(&self, _request: WorkRequest, _cpu_limit: usize, _active: usize) -> usize {
        self.widths
            .lock()
            .expect("parallel schedule lock poisoned")
            .pop_front()
            .unwrap_or(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_width_shares_cpu_between_active_executions() {
        let scheduler = ExecutionScheduler::adaptive(4);
        let first = scheduler.enter();
        let first_lease = first.try_lease(WorkRequest {
            operator: "HashJoin",
            morsels: 8,
        });
        assert_eq!(first_lease.width(), 4);

        let second = scheduler.enter();
        assert_eq!(
            second
                .try_lease(WorkRequest {
                    operator: "HashJoin",
                    morsels: 8,
                })
                .width(),
            1
        );
        drop(first_lease);
        drop(first);
        assert_eq!(
            second
                .try_lease(WorkRequest {
                    operator: "HashJoin",
                    morsels: 8,
                })
                .width(),
            4
        );
    }

    #[test]
    fn scripted_width_is_bounded_by_work_and_cpu() {
        let scheduler = ExecutionScheduler::fixed(4, [3, 8, 2]);
        let grant = scheduler.enter();
        assert_eq!(
            grant
                .try_lease(WorkRequest {
                    operator: "HashJoin",
                    morsels: 8,
                })
                .width(),
            3
        );
        assert_eq!(
            grant
                .try_lease(WorkRequest {
                    operator: "HashJoin",
                    morsels: 2,
                })
                .width(),
            2
        );
        assert_eq!(
            grant
                .try_lease(WorkRequest {
                    operator: "HashJoin",
                    morsels: 8,
                })
                .width(),
            2
        );
    }
}
