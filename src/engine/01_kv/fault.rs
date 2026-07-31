//! Traceable fault injection at Rad's ordered transactional-KV boundary.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    DataPosition, Entry, Error, ErrorKind, IsolationLevel, KeyRange, Kv, KvIterator, Result,
    Transaction, TransactionalKv,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Get,
    Put,
    Delete,
    Scan,
    IteratorNext,
    Begin,
    TransactionGet,
    TransactionPut,
    TransactionDelete,
    TransactionScan,
    Commit,
    Rollback,
    Close,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    #[default]
    None,
    Key {
        bytes: Vec<u8>,
    },
    Range {
        start: Option<Vec<u8>>,
        end: Option<Vec<u8>>,
    },
    Isolation {
        level: String,
    },
}

impl Target {
    fn key(key: &[u8]) -> Self {
        Self::Key {
            bytes: key.to_vec(),
        }
    }

    fn range(range: &KeyRange) -> Self {
        Self::Range {
            start: range.start.as_ref().map(|value| value.to_vec()),
            end: range.end.as_ref().map(|value| value.to_vec()),
        }
    }

    fn isolation(isolation: IsolationLevel) -> Self {
        Self::Isolation {
            level: isolation.as_str().into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "error", rename_all = "snake_case")]
pub enum FaultAction {
    ErrorBefore(ErrorKind),
    ErrorAfter(ErrorKind),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FaultRule {
    pub operation: Operation,
    /// One-based occurrence of this operation across the wrapper.
    pub occurrence: u64,
    pub action: FaultAction,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TracePhase {
    Started,
    Finished,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "error", rename_all = "snake_case")]
pub enum TraceOutcome {
    Pending,
    Passed,
    BackendError(ErrorKind),
    Injected(ErrorKind),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TraceEvent<T = Target> {
    pub sequence: u64,
    pub phase: TracePhase,
    pub operation: Operation,
    pub occurrence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction: Option<u64>,
    pub target: T,
    pub outcome: TraceOutcome,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RedactedTarget {
    None,
    Key {
        sha256: String,
        length: usize,
    },
    Range {
        start: Option<RedactedBytes>,
        end: Option<RedactedBytes>,
    },
    Isolation {
        level: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RedactedBytes {
    pub sha256: String,
    pub length: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RedactedTraceEvent {
    pub sequence: u64,
    pub phase: TracePhase,
    pub operation: Operation,
    pub occurrence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction: Option<u64>,
    pub target: RedactedTarget,
    pub outcome: TraceOutcome,
}

#[derive(Clone, Default)]
pub struct FaultController {
    inner: Arc<Mutex<ControlState>>,
}

#[derive(Default)]
struct ControlState {
    sequence: u64,
    next_transaction: u64,
    occurrences: HashMap<Operation, u64>,
    rules: Vec<FaultRule>,
    trace: Vec<TraceEvent>,
}

impl FaultController {
    pub fn new(rules: Vec<FaultRule>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ControlState {
                rules,
                ..ControlState::default()
            })),
        }
    }

    pub fn trace(&self) -> Vec<TraceEvent> {
        self.inner
            .lock()
            .expect("fault controller mutex poisoned")
            .trace
            .clone()
    }

    /// Return an artifact-safe trace. Keys and range bounds can contain user
    /// data, so retained evidence carries only their byte length and digest.
    pub fn redacted_trace(&self) -> Vec<RedactedTraceEvent> {
        self.trace()
            .into_iter()
            .map(|event| RedactedTraceEvent {
                sequence: event.sequence,
                phase: event.phase,
                operation: event.operation,
                occurrence: event.occurrence,
                transaction: event.transaction,
                target: redact_target(event.target),
                outcome: event.outcome,
            })
            .collect()
    }

    pub fn clear_trace(&self) {
        self.inner
            .lock()
            .expect("fault controller mutex poisoned")
            .trace
            .clear();
    }

    /// Inject a fault into the next occurrence of an operation. Semantic
    /// engine hooks use this to select the storage call following a durable
    /// boundary without relying on a brittle global occurrence number.
    pub fn inject_next(&self, operation: Operation, action: FaultAction) {
        let mut state = self.inner.lock().expect("fault controller mutex poisoned");
        let occurrence = state
            .occurrences
            .get(&operation)
            .copied()
            .unwrap_or_default()
            .saturating_add(1);
        state.rules.push(FaultRule {
            operation,
            occurrence,
            action,
        });
    }

    fn allocate_transaction(&self) -> u64 {
        let mut state = self.inner.lock().expect("fault controller mutex poisoned");
        state.next_transaction = state.next_transaction.saturating_add(1);
        state.next_transaction
    }

    fn start(&self, context: &TraceContext, operation: Operation, target: Target) -> Call {
        let mut state = self.inner.lock().expect("fault controller mutex poisoned");
        let occurrence = {
            let value = state.occurrences.entry(operation).or_default();
            *value = value.saturating_add(1);
            *value
        };
        let action = state
            .rules
            .iter()
            .find(|rule| rule.operation == operation && rule.occurrence == occurrence)
            .map(|rule| rule.action);
        push_event(
            &mut state,
            TracePhase::Started,
            operation,
            occurrence,
            context.transaction,
            target.clone(),
            TraceOutcome::Pending,
        );
        Call {
            context: context.clone(),
            operation,
            occurrence,
            target,
            action,
        }
    }
}

#[derive(Clone)]
struct TraceContext {
    controller: FaultController,
    transaction: Option<u64>,
}

impl TraceContext {
    fn store(controller: FaultController) -> Self {
        Self {
            controller,
            transaction: None,
        }
    }

    fn transaction(&self) -> Self {
        Self {
            controller: self.controller.clone(),
            transaction: Some(self.controller.allocate_transaction()),
        }
    }

    fn start(&self, operation: Operation, target: Target) -> Call {
        self.controller.start(self, operation, target)
    }
}

struct Call {
    context: TraceContext,
    operation: Operation,
    occurrence: u64,
    target: Target,
    action: Option<FaultAction>,
}

impl Call {
    fn before(&self) -> Result<()> {
        if let Some(FaultAction::ErrorBefore(kind)) = self.action {
            self.finish(TraceOutcome::Injected(kind));
            return Err(injected(self.operation, kind));
        }
        Ok(())
    }

    fn after<T>(&self, result: Result<T>) -> Result<T> {
        match result {
            Ok(_) if let Some(FaultAction::ErrorAfter(kind)) = self.action => {
                self.finish(TraceOutcome::Injected(kind));
                Err(injected(self.operation, kind))
            }
            Ok(value) => {
                self.finish(TraceOutcome::Passed);
                Ok(value)
            }
            Err(error) => {
                self.finish(TraceOutcome::BackendError(error.kind()));
                Err(error)
            }
        }
    }

    fn run<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        self.before()?;
        self.after(operation())
    }

    async fn run_async<T, F>(&self, operation: impl FnOnce() -> F) -> Result<T>
    where
        F: Future<Output = Result<T>>,
    {
        self.before()?;
        self.after(operation().await)
    }

    fn run_infallible(&self, operation: impl FnOnce()) {
        if self.before().is_ok() {
            operation();
            let _result = self.after(Ok(()));
        }
    }

    fn finish(&self, outcome: TraceOutcome) {
        let mut state = self
            .context
            .controller
            .inner
            .lock()
            .expect("fault controller mutex poisoned");
        push_event(
            &mut state,
            TracePhase::Finished,
            self.operation,
            self.occurrence,
            self.context.transaction,
            self.target.clone(),
            outcome,
        );
    }
}

fn push_event(
    state: &mut ControlState,
    phase: TracePhase,
    operation: Operation,
    occurrence: u64,
    transaction: Option<u64>,
    target: Target,
    outcome: TraceOutcome,
) {
    state.sequence = state.sequence.saturating_add(1);
    state.trace.push(TraceEvent {
        sequence: state.sequence,
        phase,
        operation,
        occurrence,
        transaction,
        target,
        outcome,
    });
}

fn injected(operation: Operation, kind: ErrorKind) -> Error {
    Error::message(
        kind,
        format!("fault injection: {operation:?} returned {kind:?}"),
    )
}

pub struct FaultingKv {
    inner: Arc<dyn TransactionalKv>,
    context: TraceContext,
}

impl FaultingKv {
    pub fn new(inner: Arc<dyn TransactionalKv>, controller: FaultController) -> Self {
        Self {
            inner,
            context: TraceContext::store(controller),
        }
    }

    pub fn controller(&self) -> &FaultController {
        &self.context.controller
    }
}

#[async_trait]
impl Kv for FaultingKv {
    async fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        let call = self.context.start(Operation::Get, Target::key(key));
        call.run_async(|| Kv::get(&*self.inner, key)).await
    }

    async fn put(&self, key: Bytes, value: Bytes) -> Result<()> {
        let call = self.context.start(Operation::Put, Target::key(&key));
        call.run_async(|| Kv::put(&*self.inner, key, value)).await
    }

    async fn delete(&self, key: &[u8]) -> Result<()> {
        let call = self.context.start(Operation::Delete, Target::key(key));
        call.run_async(|| Kv::delete(&*self.inner, key)).await
    }

    async fn scan(&self, range: KeyRange) -> Result<Box<dyn KvIterator>> {
        let call = self.context.start(Operation::Scan, Target::range(&range));
        let iterator = call.run_async(|| Kv::scan(&*self.inner, range)).await?;
        Ok(Box::new(FaultingIterator {
            inner: iterator,
            context: self.context.clone(),
        }))
    }
}

#[async_trait]
impl TransactionalKv for FaultingKv {
    async fn begin(&self, isolation: IsolationLevel) -> Result<Box<dyn Transaction>> {
        let context = self.context.transaction();
        let call = context.start(Operation::Begin, Target::isolation(isolation));
        let inner = call.run_async(|| self.inner.begin(isolation)).await?;
        Ok(Box::new(FaultingTransaction { inner, context }))
    }

    async fn close(&self) -> Result<()> {
        let call = self.context.start(Operation::Close, Target::None);
        call.run_async(|| self.inner.close()).await
    }
}

struct FaultingTransaction {
    inner: Box<dyn Transaction>,
    context: TraceContext,
}

impl FaultingTransaction {
    fn into_parts(self) -> (Box<dyn Transaction>, TraceContext) {
        let Self { inner, context } = self;
        (inner, context)
    }
}

#[async_trait]
impl Transaction for FaultingTransaction {
    fn begin_position(&self) -> &DataPosition {
        self.inner.begin_position()
    }

    async fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        let call = self
            .context
            .start(Operation::TransactionGet, Target::key(key));
        call.run_async(|| self.inner.get(key)).await
    }

    fn put(&self, key: Bytes, value: Bytes) -> Result<()> {
        let call = self
            .context
            .start(Operation::TransactionPut, Target::key(&key));
        call.run(|| self.inner.put(key, value))
    }

    fn delete(&self, key: &[u8]) -> Result<()> {
        let call = self
            .context
            .start(Operation::TransactionDelete, Target::key(key));
        call.run(|| self.inner.delete(key))
    }

    fn untrack_write(&self, key: &[u8]) -> Result<()> {
        self.inner.untrack_write(key)
    }

    async fn scan<'a>(&'a self, range: KeyRange) -> Result<Box<dyn KvIterator + 'a>> {
        let call = self
            .context
            .start(Operation::TransactionScan, Target::range(&range));
        let iterator = call.run_async(|| self.inner.scan(range)).await?;
        Ok(Box::new(FaultingIterator {
            inner: iterator,
            context: self.context.clone(),
        }))
    }

    async fn commit(self: Box<Self>) -> Result<()> {
        let (inner, context) = (*self).into_parts();
        let call = context.start(Operation::Commit, Target::None);
        call.run_async(|| inner.commit()).await
    }

    fn rollback(self: Box<Self>) {
        let (inner, context) = (*self).into_parts();
        let call = context.start(Operation::Rollback, Target::None);
        // Rollback is infallible at the trait boundary. A pre-injected fault
        // drops the transaction before explicit rollback.
        call.run_infallible(|| inner.rollback());
    }
}

struct FaultingIterator<'a> {
    inner: Box<dyn KvIterator + 'a>,
    context: TraceContext,
}

#[async_trait]
impl KvIterator for FaultingIterator<'_> {
    async fn next(&mut self) -> Result<Option<Entry>> {
        let call = self.context.start(Operation::IteratorNext, Target::None);
        call.run_async(|| self.inner.next()).await
    }
}

fn redact_target(target: Target) -> RedactedTarget {
    match target {
        Target::None => RedactedTarget::None,
        Target::Key { bytes } => {
            let bytes = redact_bytes(&bytes);
            RedactedTarget::Key {
                sha256: bytes.sha256,
                length: bytes.length,
            }
        }
        Target::Range { start, end } => RedactedTarget::Range {
            start: start.as_deref().map(redact_bytes),
            end: end.as_deref().map(redact_bytes),
        },
        Target::Isolation { level } => RedactedTarget::Isolation { level },
    }
}

fn redact_bytes(bytes: &[u8]) -> RedactedBytes {
    RedactedBytes {
        sha256: format!("sha256:{:x}", Sha256::digest(bytes)),
        length: bytes.len(),
    }
}
