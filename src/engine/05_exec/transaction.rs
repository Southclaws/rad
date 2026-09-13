use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::engine::kv::{
    DataPosition, IsolationLevel, KeyRange, KvIterator, Result as KvResult, ScanOrder, ScanRequest,
    Transaction,
};

use super::{Engine, Error, ErrorKind, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionIsolation {
    ReadCommitted,
    Serializable,
}

impl TransactionIsolation {
    fn storage_isolation(self) -> IsolationLevel {
        match self {
            Self::ReadCommitted => IsolationLevel::Snapshot,
            Self::Serializable => IsolationLevel::SerializableSnapshot,
        }
    }
}

#[derive(Clone)]
enum WriteOperation {
    Put { key: Bytes, value: Bytes },
    Delete { key: Bytes },
    Untrack { key: Bytes },
}

#[derive(Default)]
pub(super) struct WriteIntentRegistry {
    locks: Mutex<HashMap<Bytes, Weak<AsyncMutex<()>>>>,
}

impl WriteIntentRegistry {
    fn lock_for(&self, key: &Bytes) -> Arc<AsyncMutex<()>> {
        let mut locks = self.locks.lock().expect("write intent registry poisoned");
        if let Some(lock) = locks.get(key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(AsyncMutex::new(()));
        locks.insert(key.clone(), Arc::downgrade(&lock));
        lock
    }
}

pub(crate) struct EngineTransaction {
    isolation: TransactionIsolation,
    inner: Option<Box<dyn Transaction>>,
    journal: Mutex<Vec<WriteOperation>>,
    executed_statement: bool,
    intent_keys: HashSet<Bytes>,
    intent_guards: Vec<OwnedMutexGuard<()>>,
}

impl EngineTransaction {
    pub(super) async fn begin(engine: &Engine, isolation: TransactionIsolation) -> Result<Self> {
        let inner = engine.store.begin(isolation.storage_isolation()).await?;
        Ok(Self {
            isolation,
            inner: Some(inner),
            journal: Mutex::new(Vec::new()),
            executed_statement: false,
            intent_keys: HashSet::new(),
            intent_guards: Vec::new(),
        })
    }

    pub(super) fn isolation(&self) -> TransactionIsolation {
        self.isolation
    }

    pub(super) fn statement_checkpoint(&self) -> usize {
        self.journal.lock().expect("write journal poisoned").len()
    }

    pub(super) fn has_executed_statement(&self) -> bool {
        self.executed_statement
    }

    pub(super) fn finish_statement(&mut self) {
        self.executed_statement = true;
    }

    pub(super) async fn refresh(&mut self, engine: &Engine) -> Result<()> {
        let replacement = engine
            .store
            .begin(self.isolation.storage_isolation())
            .await?;
        let operations = self.journal.lock().expect("write journal poisoned").clone();
        replay(&*replacement, &operations)?;
        self.inner
            .replace(replacement)
            .expect("engine transaction remains open")
            .rollback();
        Ok(())
    }

    pub(super) async fn restart_statement(
        &mut self,
        engine: &Engine,
        checkpoint: usize,
    ) -> Result<()> {
        self.journal
            .lock()
            .expect("write journal poisoned")
            .truncate(checkpoint);
        self.refresh(engine).await
    }

    pub(super) async fn acquire_statement_intents(
        &mut self,
        registry: &WriteIntentRegistry,
        checkpoint: usize,
    ) -> Result<bool> {
        let mut keys = {
            let operations = self.journal.lock().expect("write journal poisoned");
            let untracked = operations
                .iter()
                .filter_map(|operation| match operation {
                    WriteOperation::Untrack { key } => Some(key.clone()),
                    _ => None,
                })
                .collect::<HashSet<_>>();
            operations[checkpoint..]
                .iter()
                .filter_map(|operation| match operation {
                    WriteOperation::Put { key, .. } | WriteOperation::Delete { key }
                        if !untracked.contains(key) && !self.intent_keys.contains(key) =>
                    {
                        Some(key.clone())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        keys.sort_unstable();
        keys.dedup();

        let acquired = !keys.is_empty();
        for key in keys {
            let lock = registry.lock_for(&key);
            let guard = match lock.clone().try_lock_owned() {
                Ok(guard) => guard,
                Err(_) => tokio::time::timeout(Duration::from_secs(5), lock.lock_owned())
                    .await
                    .map_err(|_| {
                        Error::message(
                            ErrorKind::Conflict,
                            "transaction write intent wait timed out",
                        )
                    })?,
            };
            self.intent_keys.insert(key);
            self.intent_guards.push(guard);
        }
        Ok(acquired)
    }

    pub(super) async fn commit_inner(mut self) -> KvResult<()> {
        let inner = self.inner.take().expect("engine transaction commits once");
        inner.commit().await
    }

    pub(crate) fn rollback_inner(mut self) {
        self.inner
            .take()
            .expect("engine transaction rolls back once")
            .rollback();
    }
}

fn replay(transaction: &dyn Transaction, operations: &[WriteOperation]) -> KvResult<()> {
    for operation in operations {
        match operation {
            WriteOperation::Put { key, value } => transaction.put(key.clone(), value.clone())?,
            WriteOperation::Delete { key } => transaction.delete(key)?,
            WriteOperation::Untrack { key } => transaction.untrack_write(key)?,
        }
    }
    Ok(())
}

#[async_trait]
impl Transaction for EngineTransaction {
    fn begin_position(&self) -> &DataPosition {
        self.inner
            .as_deref()
            .expect("engine transaction remains open")
            .begin_position()
    }

    async fn get(&self, key: &[u8]) -> KvResult<Option<Bytes>> {
        self.inner
            .as_deref()
            .expect("engine transaction remains open")
            .get(key)
            .await
    }

    fn put(&self, key: Bytes, value: Bytes) -> KvResult<()> {
        self.inner
            .as_deref()
            .expect("engine transaction remains open")
            .put(key.clone(), value.clone())?;
        self.journal
            .lock()
            .expect("write journal poisoned")
            .push(WriteOperation::Put { key, value });
        Ok(())
    }

    fn delete(&self, key: &[u8]) -> KvResult<()> {
        self.inner
            .as_deref()
            .expect("engine transaction remains open")
            .delete(key)?;
        self.journal
            .lock()
            .expect("write journal poisoned")
            .push(WriteOperation::Delete {
                key: Bytes::copy_from_slice(key),
            });
        Ok(())
    }

    fn untrack_write(&self, key: &[u8]) -> KvResult<()> {
        self.inner
            .as_deref()
            .expect("engine transaction remains open")
            .untrack_write(key)?;
        self.journal
            .lock()
            .expect("write journal poisoned")
            .push(WriteOperation::Untrack {
                key: Bytes::copy_from_slice(key),
            });
        Ok(())
    }

    async fn scan<'a>(&'a self, range: KeyRange) -> KvResult<Box<dyn KvIterator + 'a>> {
        self.inner
            .as_deref()
            .expect("engine transaction remains open")
            .scan(range)
            .await
    }

    async fn scan_ordered<'a>(
        &'a self,
        range: KeyRange,
        order: ScanOrder,
    ) -> KvResult<Box<dyn KvIterator + 'a>> {
        self.inner
            .as_deref()
            .expect("engine transaction remains open")
            .scan_ordered(range, order)
            .await
    }

    async fn scan_requested<'a>(
        &'a self,
        request: ScanRequest,
    ) -> KvResult<Box<dyn KvIterator + 'a>> {
        self.inner
            .as_deref()
            .expect("engine transaction remains open")
            .scan_requested(request)
            .await
    }

    async fn commit(self: Box<Self>) -> KvResult<()> {
        self.commit_inner().await
    }

    fn rollback(self: Box<Self>) {
        self.rollback_inner();
    }
}
