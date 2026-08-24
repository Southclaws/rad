//! Observation relay: an instance that cannot publish hands its evidence to
//! one that can.
//!
//! A reader observes real workload — in a read-serving deployment, most of it
//! — but holds no write access, so what it learns would die with the process.
//! The relay is the upward leg. It is not clustering: instances contribute
//! evidence to improve decisions, they do not coordinate to establish
//! correctness, and no failure here can affect a query result or a reader's
//! availability.
//!
//! Delivery attempts are at-least-once. Queue admission is idempotent. A batch
//! carries `(instance, boot, sequence)`. The receiver admits a sequence once
//! while it retains the source high-water mark.
//!
//! Nothing is durable. Evidence in flight is lost if the process dies or the
//! retry budget runs out, and both are counted rather than silent.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::engine::lir::fingerprint::Fingerprint;

use crate::engine::exec::observe::ProgramRecord;

use super::statistics::{QueryModel, StatisticsBatch, StatisticsSink};

/// Both ends of this channel are the same binary, so the wire form is a shared
/// type rather than a generated contract. A mismatch is rejected outright: a
/// rolling upgrade briefly runs two versions, and losing advisory batches for
/// that window is the right answer rather than translating between them.
pub const RELAY_FORMAT: u32 = 3;

/// How many times one batch is retried before it is abandoned. Retries are
/// idempotent, so the budget bounds memory rather than correctness.
const RETRY_BUDGET: u32 = 3;

/// A relayed batch is one HTTP request to a peer, not an object-store commit,
/// so it does not need the amortisation storage does. Publishing sooner
/// narrows both the delay before a reader's evidence can inform a plan and the
/// window its death would lose.
const RELAY_FLUSH_EVERY: u32 = 5;

/// Sources whose admitted sequence is remembered. A source that falls out is
/// admitted on its next batch and can duplicate one batch.
const MAX_SOURCES: usize = 64;

/// One instance's evidence, gathered since its last successful publication.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservationBatch {
    pub format: u32,
    /// Identity of the sending instance, and of the process within it. A
    /// restart takes a new boot, so a reused instance name cannot make one
    /// process's sequence hide another's.
    pub instance: String,
    pub boot: String,
    pub sequence: u64,
    /// Unix time when the batch left. Each model uses the same time base.
    pub sent_at_micros: u64,
    pub families: Vec<QueryModel>,
    pub frequency: Vec<(Fingerprint, u32)>,
    /// Canonical programs, carried only over a confidential transport. Empty
    /// otherwise, and empty whenever corpus capture is off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub corpus: Vec<ProgramRecord>,
}

impl ObservationBatch {
    /// How long before the batch was sent this family was last observed.
    pub(super) fn age_of(&self, model: &QueryModel) -> Duration {
        Duration::from_micros(self.sent_at_micros).saturating_sub(model.last_seen)
    }
}

/// Why a submission did not land. The distinction that matters is whether
/// sending the same bytes again could ever work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportError {
    /// The receiver could not be reached, or did not answer.
    Unavailable,
    /// The receiver is saturated and asked for less.
    Backpressured,
    /// The receiver understood and refused. Retrying cannot help.
    Rejected,
}

impl TransportError {
    fn retryable(self) -> bool {
        !matches!(self, Self::Rejected)
    }
}

/// How a batch reaches another instance. The statistics layer knows only this,
/// so HTTP, an in-process handoff, and a simulated network are all the same to
/// it — which is what keeps the relay testable without sockets.
#[async_trait::async_trait]
pub trait ObservationTransport: Send + Sync {
    async fn submit(&self, batch: &ObservationBatch) -> Result<(), TransportError>;

    /// Whether this transport is safe to carry user values.
    ///
    /// Aggregate evidence is metadata — fingerprints hash literals to typed
    /// placeholders — so it may cross an authenticated plaintext hop inside a
    /// cluster. Corpus documents are canonical PIR and contain the literals
    /// themselves, so they may not.
    ///
    /// The default is false. A transport that cannot demonstrate
    /// confidentiality must not be assumed to have it.
    fn is_confidential(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RelayCounters {
    pub sent: u64,
    pub abandoned: u64,
    pub rejected: u64,
    pub attempts_pending: u32,
    pub holding: bool,
    /// Corpus documents carried, and documents withheld because the transport
    /// could not be shown to be confidential. A withheld document is dropped:
    /// an instance that cannot publish has nowhere else to put it.
    pub corpus_sent: u64,
    pub corpus_withheld: u64,
}

/// What a relaying instance is doing, reduced to the one word an operator
/// needs before reading the counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelayHealth {
    /// The last attempt was accepted.
    Connected,
    /// A batch is held for retry after a failure.
    Retrying,
    /// A batch was given up on. Evidence has been lost.
    Losing,
    /// Nothing has been attempted yet.
    Idle,
}

impl RelayHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Retrying => "retrying",
            Self::Losing => "losing",
            Self::Idle => "idle",
        }
    }
}

impl RelayCounters {
    /// A held batch is the live signal, because it says the next attempt has
    /// something to retry. Abandonment outranks it: evidence that was dropped
    /// is worse news than evidence still in hand, and stays visible until a
    /// later send succeeds.
    pub fn health(self) -> RelayHealth {
        match self {
            _ if self.holding => RelayHealth::Retrying,
            _ if self.abandoned > 0 || self.rejected > 0 => RelayHealth::Losing,
            _ if self.sent > 0 => RelayHealth::Connected,
            _ => RelayHealth::Idle,
        }
    }
}

struct RelayState {
    sequence: u64,
    /// A batch the receiver has not acknowledged. It is retried unchanged, so
    /// its sequence stays fixed and the receiver can recognise it.
    unacknowledged: Option<ObservationBatch>,
    attempts: u32,
    counters: RelayCounters,
}

/// The publishing seam for an instance with no write access: its evidence goes
/// to another instance instead of to storage.
///
/// Exactly one place owns any piece of evidence. While a batch is held here
/// awaiting acknowledgement, the registry has already dropped it; while the
/// registry holds it, nothing here refers to it. That is why a held batch is
/// never merged with newer evidence — doing so would change what a sequence
/// means, and the receiver identifies batches by sequence.
pub struct RelaySink {
    transport: Arc<dyn ObservationTransport>,
    runtime: Arc<dyn crate::runtime::RuntimeEffects>,
    instance: String,
    boot: String,
    state: std::sync::Mutex<RelayState>,
}

impl RelaySink {
    pub fn new(
        transport: Arc<dyn ObservationTransport>,
        runtime: Arc<dyn crate::runtime::RuntimeEffects>,
        instance: String,
    ) -> Self {
        let boot = runtime.new_uuid().to_string();
        Self {
            transport,
            runtime,
            instance,
            boot,
            state: std::sync::Mutex::new(RelayState {
                sequence: 0,
                unacknowledged: None,
                attempts: 0,
                counters: RelayCounters::default(),
            }),
        }
    }

    pub fn counters(&self) -> RelayCounters {
        let state = self.state.lock().expect("relay state");
        let mut counters = state.counters;
        counters.attempts_pending = state.attempts;
        counters.holding = state.unacknowledged.is_some();
        counters
    }

    fn encode(&self, batch: &StatisticsBatch) -> ObservationBatch {
        // Decided per batch rather than once at construction: a transport
        // reloads its trust anchors, and the answer is only meaningful for the
        // batch about to be sent.
        let confidential = self.transport.is_confidential();
        let mut state = self.state.lock().expect("relay state");
        state.sequence += 1;
        let corpus = if confidential {
            batch.programs.clone()
        } else {
            state.counters.corpus_withheld += batch.programs.len() as u64;
            Vec::new()
        };
        ObservationBatch {
            format: RELAY_FORMAT,
            instance: self.instance.clone(),
            boot: self.boot.clone(),
            sequence: state.sequence,
            sent_at_micros: self.runtime.unix_time().as_micros() as u64,
            // A reader relays neither synopses nor table changes: surveying
            // belongs to the writer, and a reader mutates nothing.
            families: batch
                .models
                .iter()
                .map(|(_, model)| model.clone())
                .collect(),
            frequency: batch.frequency.clone(),
            corpus,
        }
    }

    /// Retry the held batch. Returns whether the sink is now free to send
    /// something new.
    async fn settle_held(&self) -> bool {
        let held = {
            let mut state = self.state.lock().expect("relay state");
            state.unacknowledged.take()
        };
        let Some(held) = held else {
            return true;
        };
        match self.transport.submit(&held).await {
            Ok(()) => {
                let mut state = self.state.lock().expect("relay state");
                state.counters.sent += 1;
                state.counters.corpus_sent += held.corpus.len() as u64;
                state.attempts = 0;
                true
            }
            Err(error) if error.retryable() => {
                let mut state = self.state.lock().expect("relay state");
                state.attempts += 1;
                if state.attempts > RETRY_BUDGET {
                    state.counters.abandoned += 1;
                    state.attempts = 0;
                    true
                } else {
                    state.unacknowledged = Some(held);
                    false
                }
            }
            Err(_) => {
                let mut state = self.state.lock().expect("relay state");
                state.counters.rejected += 1;
                state.attempts = 0;
                true
            }
        }
    }
}

#[async_trait::async_trait]
impl StatisticsSink for RelaySink {
    fn flush_every(&self, configured: u32) -> u32 {
        configured.min(RELAY_FLUSH_EVERY)
    }

    fn relay_counters(&self) -> Option<RelayCounters> {
        Some(self.counters())
    }

    fn carries_user_values(&self) -> bool {
        self.transport.is_confidential()
    }

    async fn publish(&self, batch: StatisticsBatch) -> Result<(), StatisticsBatch> {
        if !self.settle_held().await {
            // Still stuck on the previous batch. The registry keeps this one,
            // where it merges forward into whatever arrives next.
            return Err(batch);
        }
        if batch.carries_no_evidence() {
            return Ok(());
        }
        let wire = self.encode(&batch);
        match self.transport.submit(&wire).await {
            Ok(()) => {
                let mut state = self.state.lock().expect("relay state");
                state.counters.sent += 1;
                state.counters.corpus_sent += wire.corpus.len() as u64;
                Ok(())
            }
            Err(error) if error.retryable() => {
                let mut state = self.state.lock().expect("relay state");
                state.unacknowledged = Some(wire);
                state.attempts = 1;
                Ok(())
            }
            Err(_) => {
                self.state.lock().expect("relay state").counters.rejected += 1;
                Ok(())
            }
        }
    }

    fn has_pending_publication(&self) -> bool {
        self.state
            .lock()
            .expect("relay state")
            .unacknowledged
            .is_some()
    }
}

/// What the receiver did with a batch. The internal listener maps these to
/// status codes; nothing else needs to know they exist.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IngestOutcome {
    /// Queued to be folded into the models.
    Accepted,
    /// This source has already had this sequence admitted.
    AlreadyAdmitted,
    /// A different version of the wire form.
    FormatMismatch,
    /// This receiver does not permit workload corpus capture.
    CorpusDisabled,
    /// A corpus document exceeds the per-program limit.
    CorpusOversize,
    /// The queue is full. The sender should retry the same batch later.
    Saturated,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct IngestCounters {
    pub accepted: u64,
    pub already_admitted: u64,
    pub format_mismatch: u64,
    pub saturated: u64,
    pub corpus_oversize: u64,
    pub sources: usize,
}

struct AdmittedSource {
    sequence: u64,
    ordinal: u64,
}

/// The receiving end. It answers each submission immediately, because a sender
/// must learn whether to retry, and hands accepted batches to the runner to
/// fold in.
pub struct RelayIngest {
    sender: mpsc::Sender<ObservationBatch>,
    accept_corpus: bool,
    admitted: std::sync::Mutex<HashMap<(String, String), AdmittedSource>>,
    ordinal: AtomicU64,
    accepted: AtomicU64,
    already_admitted: AtomicU64,
    format_mismatch: AtomicU64,
    saturated: AtomicU64,
    corpus_oversize: AtomicU64,
}

impl RelayIngest {
    pub fn channel(capacity: usize) -> (Arc<Self>, mpsc::Receiver<ObservationBatch>) {
        Self::channel_with_corpus(capacity, false)
    }

    pub fn channel_with_corpus(
        capacity: usize,
        accept_corpus: bool,
    ) -> (Arc<Self>, mpsc::Receiver<ObservationBatch>) {
        let (sender, receiver) = mpsc::channel(capacity);
        let ingest = Arc::new(Self {
            sender,
            accept_corpus,
            admitted: std::sync::Mutex::new(HashMap::new()),
            ordinal: AtomicU64::new(0),
            accepted: AtomicU64::new(0),
            already_admitted: AtomicU64::new(0),
            format_mismatch: AtomicU64::new(0),
            saturated: AtomicU64::new(0),
            corpus_oversize: AtomicU64::new(0),
        });
        (ingest, receiver)
    }

    /// Accept a batch, or say why not.
    ///
    /// The admitted sequence advances only once the batch is queued. A
    /// batch that could not be queued therefore leaves no trace, so the sender
    /// retrying it is the sender recovering rather than the receiver losing it.
    pub fn submit(&self, batch: ObservationBatch) -> IngestOutcome {
        if batch.format != RELAY_FORMAT {
            self.format_mismatch.fetch_add(1, Ordering::Relaxed);
            return IngestOutcome::FormatMismatch;
        }
        if !self.accept_corpus && !batch.corpus.is_empty() {
            return IngestOutcome::CorpusDisabled;
        }
        if batch
            .corpus
            .iter()
            .any(|record| record.canonical.len() > super::statistics::CORPUS_MAX_PROGRAM_BYTES)
        {
            self.corpus_oversize.fetch_add(1, Ordering::Relaxed);
            return IngestOutcome::CorpusOversize;
        }
        let source = (batch.instance.clone(), batch.boot.clone());
        let sequence = batch.sequence;
        let mut admitted = self.admitted.lock().expect("relay sources");
        if let Some(seen) = admitted.get(&source)
            && sequence <= seen.sequence
        {
            self.already_admitted.fetch_add(1, Ordering::Relaxed);
            return IngestOutcome::AlreadyAdmitted;
        }
        if self.sender.try_send(batch).is_err() {
            self.saturated.fetch_add(1, Ordering::Relaxed);
            return IngestOutcome::Saturated;
        }
        let ordinal = self.ordinal.fetch_add(1, Ordering::Relaxed);
        if !admitted.contains_key(&source) && admitted.len() >= MAX_SOURCES {
            let coldest = admitted
                .iter()
                .min_by_key(|(_, seen)| seen.ordinal)
                .map(|(key, _)| key.clone());
            if let Some(coldest) = coldest {
                admitted.remove(&coldest);
            }
        }
        admitted.insert(source, AdmittedSource { sequence, ordinal });
        self.accepted.fetch_add(1, Ordering::Relaxed);
        IngestOutcome::Accepted
    }

    pub fn counters(&self) -> IngestCounters {
        IngestCounters {
            accepted: self.accepted.load(Ordering::Relaxed),
            already_admitted: self.already_admitted.load(Ordering::Relaxed),
            format_mismatch: self.format_mismatch.load(Ordering::Relaxed),
            saturated: self.saturated.load(Ordering::Relaxed),
            corpus_oversize: self.corpus_oversize.load(Ordering::Relaxed),
            sources: self.admitted.lock().expect("relay sources").len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crate::engine::planner::models::ObservationModelKind;

    use super::super::statistics::{HotRegistry, StatisticsRunner};
    use super::*;

    /// Records everything submitted and answers however the test says. Both
    /// ends of the relay are in one process, which is the point: the seam
    /// exists so the relay can be exercised without a network.
    struct ScriptedTransport {
        delivered: Mutex<Vec<ObservationBatch>>,
        outcomes: Mutex<Vec<Result<(), TransportError>>>,
        ingest: Option<Arc<RelayIngest>>,
        confidential: std::sync::atomic::AtomicBool,
    }

    impl ScriptedTransport {
        fn always_ok(ingest: Arc<RelayIngest>) -> Arc<Self> {
            Arc::new(Self {
                delivered: Mutex::new(Vec::new()),
                outcomes: Mutex::new(Vec::new()),
                ingest: Some(ingest),
                confidential: std::sync::atomic::AtomicBool::new(false),
            })
        }

        fn scripted(outcomes: Vec<Result<(), TransportError>>) -> Arc<Self> {
            Arc::new(Self {
                delivered: Mutex::new(Vec::new()),
                outcomes: Mutex::new(outcomes),
                ingest: None,
                confidential: std::sync::atomic::AtomicBool::new(false),
            })
        }

        fn delivered(&self) -> Vec<ObservationBatch> {
            self.delivered.lock().expect("delivered").clone()
        }
    }

    #[async_trait::async_trait]
    impl ObservationTransport for ScriptedTransport {
        fn is_confidential(&self) -> bool {
            self.confidential.load(Ordering::Relaxed)
        }

        async fn submit(&self, batch: &ObservationBatch) -> Result<(), TransportError> {
            self.delivered
                .lock()
                .expect("delivered")
                .push(batch.clone());
            let scripted = {
                let mut outcomes = self.outcomes.lock().expect("outcomes");
                if outcomes.is_empty() {
                    None
                } else {
                    Some(outcomes.remove(0))
                }
            };
            // A scripted failure stands in for the network. A scripted
            // success still has to be delivered, or the test would prove the
            // sender's bookkeeping and nothing about the receiver.
            if let Some(Err(error)) = scripted {
                return Err(error);
            }
            match &self.ingest {
                Some(ingest) => match ingest.submit(batch.clone()) {
                    IngestOutcome::Accepted | IngestOutcome::AlreadyAdmitted => Ok(()),
                    IngestOutcome::Saturated => Err(TransportError::Backpressured),
                    IngestOutcome::FormatMismatch
                    | IngestOutcome::CorpusDisabled
                    | IngestOutcome::CorpusOversize => Err(TransportError::Rejected),
                },
                None => Ok(()),
            }
        }
    }

    fn sink(transport: Arc<dyn ObservationTransport>) -> RelaySink {
        RelaySink::new(
            transport,
            Arc::new(crate::runtime::SystemRuntime),
            "reader-1".into(),
        )
    }

    fn batch(models: Vec<(Fingerprint, QueryModel)>) -> StatisticsBatch {
        StatisticsBatch {
            models,
            ..StatisticsBatch::default()
        }
    }

    fn family(seed: u8) -> Fingerprint {
        use crate::engine::lir::fingerprint::{CANONICALIZATION_VERSION, HASH_SHA256_128};
        Fingerprint {
            canonicalization_version: CANONICALIZATION_VERSION,
            hash_algorithm: HASH_SHA256_128,
            digest: [seed; 16],
        }
    }

    fn model(seed: u8, executions: u64) -> QueryModel {
        let mut model = QueryModel::new(family(seed), ObservationModelKind::Statement);
        for _ in 0..executions {
            model.rows.record(40);
            model
                .resources
                .record(crate::engine::planner::models::KvResourceSample {
                    gets: u64::from(seed),
                    scans: 1,
                    iterated: 40,
                    bytes_read: 400,
                    ..Default::default()
                });
            model.executions += 1;
        }
        model
    }

    fn document(seed: u8) -> ProgramRecord {
        ProgramRecord {
            canonical: format!(r#"{{"literal":"secret-{seed}"}}"#).into_bytes(),
            content_hash: [seed; 16],
            at_unix_micros: 1_700_000_000_000_000,
            statements: 1,
            outcomes: Vec::new(),
        }
    }

    /// Corpus documents are canonical PIR and contain the literal values of
    /// the programs that ran. Aggregate evidence does not — fingerprints hash
    /// literals to typed placeholders — so the two have different rules, and
    /// only the aggregates may cross a channel that is merely authenticated.
    #[tokio::test]
    async fn a_plaintext_transport_carries_evidence_but_never_user_values() {
        let transport = ScriptedTransport::scripted(Vec::new());
        assert!(
            !transport.is_confidential(),
            "the default transport claimed confidentiality"
        );
        let sink = sink(transport.clone());

        let batch = StatisticsBatch {
            models: vec![(family(1), model(1, 5))],
            programs: vec![document(1), document(2)],
            ..StatisticsBatch::default()
        };
        assert!(sink.publish(batch).await.is_ok());

        let sent = transport.delivered().pop().expect("one batch");
        assert!(
            sent.corpus.is_empty(),
            "user values crossed a transport that is not confidential"
        );
        // The aggregate evidence still travels: withholding it would cost the
        // planner for a reason that does not apply to it.
        assert_eq!(sent.families.len(), 1);

        let counters = sink.counters();
        assert_eq!(counters.corpus_withheld, 2);
        assert_eq!(counters.corpus_sent, 0);
    }

    /// A confidential transport carries them, and the receiver adopts them
    /// into the corpus it already stores.
    #[tokio::test]
    async fn a_confidential_transport_carries_user_values() {
        let (ingest, mut received) = RelayIngest::channel_with_corpus(8, true);
        let transport = ScriptedTransport::always_ok(ingest.clone());
        transport.confidential.store(true, Ordering::Relaxed);
        let sink = sink(transport.clone());

        let batch = StatisticsBatch {
            models: vec![(family(1), model(1, 5))],
            programs: vec![document(1), document(2)],
            ..StatisticsBatch::default()
        };
        assert!(sink.publish(batch).await.is_ok());
        assert_eq!(sink.counters().corpus_sent, 2);
        assert_eq!(sink.counters().corpus_withheld, 0);

        let delivered = received.try_recv().expect("a batch arrived");
        assert_eq!(delivered.corpus.len(), 2);

        let mut registry = HotRegistry::new(64);
        for record in delivered.corpus {
            registry.absorb_relayed_program(record);
        }
        let published = registry.take_batch();
        assert_eq!(
            published.programs.len(),
            2,
            "adopted documents never reached the store"
        );
    }

    /// The word an operator reads first has to distinguish "in hand" from
    /// "gone". A held batch is still recoverable; an abandoned one is evidence
    /// already lost, and that stays visible rather than being cleared by the
    /// next success.
    #[test]
    fn relay_health_separates_evidence_in_hand_from_evidence_lost() {
        let idle = RelayCounters::default();
        assert_eq!(idle.health(), RelayHealth::Idle);

        let sending = RelayCounters {
            sent: 3,
            ..RelayCounters::default()
        };
        assert_eq!(sending.health(), RelayHealth::Connected);

        let holding = RelayCounters {
            sent: 3,
            holding: true,
            ..RelayCounters::default()
        };
        assert_eq!(holding.health(), RelayHealth::Retrying);

        // Abandonment outranks a healthy send count: the evidence it dropped
        // is not recovered by later batches succeeding.
        let lost = RelayCounters {
            sent: 100,
            abandoned: 1,
            ..RelayCounters::default()
        };
        assert_eq!(lost.health(), RelayHealth::Losing);
        let refused = RelayCounters {
            sent: 100,
            rejected: 1,
            ..RelayCounters::default()
        };
        assert_eq!(refused.health(), RelayHealth::Losing);

        // Holding outranks abandonment: the immediate state is what an
        // operator acts on, and the loss is still counted beside it.
        let both = RelayCounters {
            sent: 1,
            abandoned: 1,
            holding: true,
            ..RelayCounters::default()
        };
        assert_eq!(both.health(), RelayHealth::Retrying);
    }

    /// The property the whole design rests on: retrying is free. A receiver
    /// that already admitted a sequence must ignore it, so the sender never has
    /// to know whether an ambiguous failure landed.
    #[tokio::test]
    async fn a_replayed_batch_enters_the_queue_once() {
        let (ingest, mut received) = RelayIngest::channel(8);
        let transport = ScriptedTransport::always_ok(ingest.clone());
        let sink = sink(transport.clone());

        assert!(
            sink.publish(batch(vec![(family(1), model(1, 5))]))
                .await
                .is_ok()
        );
        let sent = transport.delivered().pop().expect("one batch");

        for _ in 0..4 {
            assert_eq!(ingest.submit(sent.clone()), IngestOutcome::AlreadyAdmitted);
        }
        assert_eq!(ingest.counters().accepted, 1);
        assert_eq!(ingest.counters().already_admitted, 4);

        let mut merged = QueryModel::new(family(1), ObservationModelKind::Statement);
        while let Ok(batch) = received.try_recv() {
            for model in &batch.families {
                merged.merge(model);
            }
        }
        assert_eq!(
            merged.executions, 5,
            "replaying a batch entered the queue more than once"
        );
    }

    #[tokio::test]
    async fn concurrent_duplicates_enter_the_queue_once() {
        let (ingest, mut received) = RelayIngest::channel(32);
        let batch = ObservationBatch {
            format: RELAY_FORMAT,
            instance: "reader-1".into(),
            boot: "boot".into(),
            sequence: 1,
            sent_at_micros: 0,
            families: vec![model(1, 3)],
            frequency: Vec::new(),
            corpus: Vec::new(),
        };
        let barrier = Arc::new(tokio::sync::Barrier::new(16));
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let ingest = ingest.clone();
            let batch = batch.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                ingest.submit(batch)
            }));
        }

        let mut accepted = 0;
        for task in tasks {
            if task.await.unwrap() == IngestOutcome::Accepted {
                accepted += 1;
            }
        }
        assert_eq!(accepted, 1);
        received.try_recv().expect("accepted batch");
        assert!(received.try_recv().is_err());
    }

    /// A batch that could not be queued must leave no trace, or the sender
    /// would retry something the receiver has recorded as admitted and the
    /// evidence would vanish.
    #[tokio::test]
    async fn a_batch_refused_for_saturation_can_still_be_retried() {
        let (ingest, mut received) = RelayIngest::channel(1);
        let first = ObservationBatch {
            format: RELAY_FORMAT,
            instance: "reader-1".into(),
            boot: "boot".into(),
            sequence: 1,
            sent_at_micros: 0,
            families: vec![model(1, 3)],
            frequency: Vec::new(),
            corpus: Vec::new(),
        };
        let second = ObservationBatch {
            sequence: 2,
            ..first.clone()
        };
        assert_eq!(ingest.submit(first), IngestOutcome::Accepted);
        assert_eq!(ingest.submit(second.clone()), IngestOutcome::Saturated);

        // Draining makes room, and the refused batch is still welcome.
        received.try_recv().expect("the queued batch");
        assert_eq!(ingest.submit(second), IngestOutcome::Accepted);
    }

    #[tokio::test]
    async fn a_different_wire_version_is_refused_rather_than_translated() {
        let (ingest, _received) = RelayIngest::channel(8);
        let batch = ObservationBatch {
            format: RELAY_FORMAT + 1,
            instance: "reader-1".into(),
            boot: "boot".into(),
            sequence: 1,
            sent_at_micros: 0,
            families: vec![model(1, 3)],
            frequency: Vec::new(),
            corpus: Vec::new(),
        };
        assert_eq!(ingest.submit(batch), IngestOutcome::FormatMismatch);
        assert_eq!(ingest.counters().format_mismatch, 1);
    }

    #[test]
    fn a_receiver_rejects_corpus_when_capture_is_disabled() {
        let (ingest, mut received) = RelayIngest::channel(8);
        let mut submitted = ObservationBatch {
            format: RELAY_FORMAT,
            instance: "reader-1".into(),
            boot: "boot".into(),
            sequence: 1,
            sent_at_micros: 0,
            families: Vec::new(),
            frequency: Vec::new(),
            corpus: vec![ProgramRecord {
                canonical: br#"{"statements":[]}"#.to_vec(),
                content_hash: [1; 16],
                at_unix_micros: 1,
                statements: 0,
                outcomes: Vec::new(),
            }],
        };
        assert_eq!(
            ingest.submit(submitted.clone()),
            IngestOutcome::CorpusDisabled
        );
        assert!(received.try_recv().is_err());

        let (ingest, mut received) = RelayIngest::channel_with_corpus(8, true);
        submitted.sequence = 2;
        assert_eq!(ingest.submit(submitted), IngestOutcome::Accepted);
        assert_eq!(received.try_recv().expect("corpus batch").corpus.len(), 1);
    }

    #[test]
    fn a_receiver_counts_an_oversize_corpus_document() {
        let (ingest, mut received) = RelayIngest::channel_with_corpus(8, true);
        let submitted = ObservationBatch {
            format: RELAY_FORMAT,
            instance: "reader-1".into(),
            boot: "boot".into(),
            sequence: 1,
            sent_at_micros: 0,
            families: Vec::new(),
            frequency: Vec::new(),
            corpus: vec![ProgramRecord {
                canonical: vec![0; super::super::statistics::CORPUS_MAX_PROGRAM_BYTES + 1],
                content_hash: [1; 16],
                at_unix_micros: 1,
                statements: 1,
                outcomes: Vec::new(),
            }],
        };

        assert_eq!(ingest.submit(submitted), IngestOutcome::CorpusOversize);
        assert_eq!(ingest.counters().corpus_oversize, 1);
        assert!(received.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_corpus_only_batch_uses_a_confidential_transport() {
        let transport = ScriptedTransport::scripted(Vec::new());
        transport.confidential.store(true, Ordering::Relaxed);
        let sink = sink(transport.clone());
        let batch = StatisticsBatch {
            programs: vec![ProgramRecord {
                canonical: br#"{"statements":[]}"#.to_vec(),
                content_hash: [2; 16],
                at_unix_micros: 2,
                statements: 0,
                outcomes: Vec::new(),
            }],
            ..StatisticsBatch::default()
        };

        assert!(sink.publish(batch).await.is_ok());
        let delivered = transport.delivered();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].corpus.len(), 1);
    }

    #[tokio::test]
    async fn corpus_sent_counts_receiver_admission() {
        let transport = ScriptedTransport::scripted(vec![Err(TransportError::Unavailable), Ok(())]);
        transport.confidential.store(true, Ordering::Relaxed);
        let sink = sink(transport);
        let batch = StatisticsBatch {
            programs: vec![document(3)],
            ..StatisticsBatch::default()
        };

        assert!(sink.publish(batch).await.is_ok());
        assert_eq!(sink.counters().corpus_sent, 0);
        assert!(sink.publish(StatisticsBatch::default()).await.is_ok());
        assert_eq!(sink.counters().corpus_sent, 1);
    }

    /// A failing transport must cost latency, not evidence: the sender holds
    /// the batch and retries it unchanged, and the registry keeps accumulating
    /// behind it.
    #[tokio::test]
    async fn an_unacknowledged_batch_is_retried_unchanged() {
        let transport = ScriptedTransport::scripted(vec![
            Err(TransportError::Unavailable),
            Err(TransportError::Unavailable),
            Ok(()),
        ]);
        let sink = sink(transport.clone());

        assert!(
            sink.publish(batch(vec![(family(1), model(1, 5))]))
                .await
                .is_ok()
        );
        assert!(sink.counters().holding, "the failed batch was not held");

        // The registry keeps its newer evidence while the sink is stuck.
        assert!(
            sink.publish(batch(vec![(family(2), model(2, 2))]))
                .await
                .is_err()
        );
        assert!(
            sink.publish(batch(vec![(family(2), model(2, 3))]))
                .await
                .is_ok()
        );

        let delivered = transport.delivered();
        assert_eq!(delivered[0].sequence, delivered[1].sequence);
        assert_eq!(delivered[0].families[0].executions, 5);
        assert_eq!(delivered[1].families[0].executions, 5);
        assert!(!sink.counters().holding);
        assert_eq!(
            sink.counters().sent,
            2,
            "the held batch and then the new one"
        );
    }

    #[tokio::test]
    async fn the_runner_retries_a_held_batch_without_new_evidence() {
        use crate::engine::exec::observe::ExecutionObserver as _;

        let transport = ScriptedTransport::scripted(vec![Err(TransportError::Unavailable), Ok(())]);
        let sink = Arc::new(sink(transport.clone()));
        let runner = StatisticsRunner::start(
            Arc::new(crate::runtime::SystemRuntime),
            super::super::statistics::StatisticsConfig {
                publish_interval: Duration::from_millis(10),
                flush_every: 1,
                ..Default::default()
            },
            None,
            Some(sink.clone()),
        );
        runner
            .collector()
            .statement(crate::scheduler::statistics::tests::observation(1, 1, 1));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while sink.counters().sent == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the held batch was not retried"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(transport.delivered().len(), 2);
        assert!(!sink.counters().holding);
        runner.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_drains_a_held_batch() {
        let transport = ScriptedTransport::scripted(vec![
            Err(TransportError::Unavailable),
            Err(TransportError::Unavailable),
            Ok(()),
        ]);
        let sink = Arc::new(sink(transport));
        assert!(
            sink.publish(batch(vec![(family(1), model(1, 1))]))
                .await
                .is_ok()
        );
        assert!(sink.counters().holding);

        let runner = StatisticsRunner::start(
            Arc::new(crate::runtime::SystemRuntime),
            super::super::statistics::StatisticsConfig::default(),
            None,
            Some(sink.clone()),
        );
        runner.shutdown().await;

        assert_eq!(sink.counters().sent, 1);
        assert!(!sink.counters().holding);
    }

    /// An unreachable receiver must not grow the sender without bound. After
    /// the budget the batch is abandoned and counted, and the sender carries
    /// on serving.
    #[tokio::test]
    async fn a_batch_is_abandoned_once_the_retry_budget_runs_out() {
        let transport = ScriptedTransport::scripted(vec![Err(TransportError::Unavailable); 16]);
        let sink = sink(transport.clone());

        assert!(
            sink.publish(batch(vec![(family(1), model(1, 5))]))
                .await
                .is_ok()
        );
        assert!(sink.counters().holding);

        let mut attempts = 0;
        while sink.counters().abandoned == 0 {
            attempts += 1;
            assert!(
                attempts <= RETRY_BUDGET + 2,
                "the batch was retried forever"
            );
            // While a batch is held, the registry keeps its newer evidence:
            // the sink refuses to take on a second thing it might lose.
            let outcome = sink.publish(batch(vec![(family(2), model(2, 1))])).await;
            assert_eq!(
                outcome.is_err(),
                sink.counters().abandoned == 0,
                "evidence was neither kept by the registry nor taken by the sink"
            );
        }
        assert!(
            attempts > 1,
            "the batch was abandoned without being retried"
        );
    }

    #[tokio::test]
    async fn a_refused_batch_is_not_retried() {
        let transport = ScriptedTransport::scripted(vec![Err(TransportError::Rejected)]);
        let sink = sink(transport.clone());
        assert!(
            sink.publish(batch(vec![(family(1), model(1, 5))]))
                .await
                .is_ok()
        );
        assert!(
            !sink.counters().holding,
            "a batch the receiver refused was kept for retry"
        );
        assert_eq!(sink.counters().rejected, 1);
    }

    /// Relayed evidence must reach the receiver's models, and its own delta,
    /// so that what a reader observed is both planned from and published
    /// onwards by the writer.
    #[tokio::test]
    async fn relayed_evidence_joins_the_models_and_the_next_publication() {
        let mut registry = HotRegistry::new(16);
        let now = Duration::from_secs(100);
        registry.merge_relayed(family(1), &model(1, 7), Duration::from_secs(2), now);
        registry.merge_relayed_frequency(family(2), 9);

        let resident = registry
            .model(ObservationModelKind::Statement, &family(1))
            .expect("relayed family");
        assert_eq!(resident.executions, 7);
        assert_eq!(
            resident
                .resources
                .cost()
                .expect("relayed resource cost")
                .observed_executions,
            7
        );
        assert_eq!(
            resident.last_seen,
            Duration::from_secs(98),
            "the remote observation age was not rebased"
        );
        assert!(registry.frequency(&family(2)) >= 9);

        let published = registry.take_batch();
        let (_, delta) = published
            .models
            .iter()
            .find(|(seen, _)| *seen == family(1))
            .expect("relayed evidence awaiting publication");
        assert_eq!(delta.executions, 7);
        assert_eq!(
            delta
                .resources
                .cost()
                .expect("published resource cost")
                .gets
                .maximum,
            1
        );
        assert!(
            published
                .frequency
                .iter()
                .any(|(seen, count)| *seen == family(2) && *count == 9)
        );
    }

    /// The asymmetry that protects local evidence: observing a redefinition
    /// restarts a model, but a lagging instance's report must not.
    #[tokio::test]
    async fn relayed_evidence_from_a_different_catalog_is_dropped() {
        use crate::engine::planner::models::DependencyStamp;

        let mut registry = HotRegistry::new(16);
        let now = Duration::from_secs(10);
        let mut current = model(1, 4);
        current.stamp = DependencyStamp {
            semantic: 2,
            access: 0,
        };
        registry.merge_relayed(family(1), &current, Duration::ZERO, now);

        let mut stale = model(1, 100);
        stale.stamp = DependencyStamp {
            semantic: 1,
            access: 0,
        };
        registry.merge_relayed(family(1), &stale, Duration::ZERO, now);

        assert_eq!(
            registry
                .model(ObservationModelKind::Statement, &family(1))
                .expect("resident")
                .executions,
            4,
            "a lagging instance's evidence displaced what this instance holds"
        );

        for seed in 2..40 {
            registry.merge_relayed(family(seed), &model(seed, 100), Duration::ZERO, now);
        }
        assert!(
            registry
                .model(ObservationModelKind::Statement, &family(1))
                .is_none()
        );
        registry.merge_relayed(family(1), &stale, Duration::ZERO, now);
        assert!(
            registry
                .model(ObservationModelKind::Statement, &family(1))
                .is_none(),
            "eviction removed the stamp that rejects stale evidence"
        );
    }

    /// The whole point, end to end and in one process: what a reader observed
    /// must arrive at the writer as the model the writer would have built had
    /// it served that traffic itself. Publications fail and are retried along
    /// the way, because that must not change the answer.
    #[tokio::test]
    async fn a_readers_workload_reaches_the_writer_intact() {
        use crate::engine::exec::observe::ExecutionObserver as _;

        let (ingest, mut received) = RelayIngest::channel(64);
        let transport = ScriptedTransport::always_ok(ingest.clone());
        // Every third attempt fails, so the run exercises holding, retrying,
        // and the registry accumulating behind a stuck sink.
        {
            let mut outcomes = transport.outcomes.lock().expect("outcomes");
            for index in 0..24 {
                outcomes.push(if index % 3 == 2 {
                    Err(TransportError::Unavailable)
                } else {
                    Ok(())
                });
            }
        }
        let sink = sink(transport.clone());

        // What the reader served, and what a writer serving it would hold.
        let (collector, mut observations) =
            super::super::statistics::StatisticsCollector::channel(256);
        let mut reader = HotRegistry::new(64);
        let mut alone = HotRegistry::new(64);
        for index in 0..30u8 {
            collector.statement(crate::scheduler::statistics::tests::observation(
                1,
                index,
                10 * u64::from(index),
            ));
        }
        let at = Duration::from_millis(200);
        let mut published = 0;
        while let Ok(event) = observations.try_recv() {
            if let super::super::statistics::StatisticsEvent::Statement(observed) = event {
                reader.absorb(&observed, at);
                alone.absorb(&observed, at);
                published += 1;
                if published % 4 == 0 {
                    let batch = reader.take_batch();
                    if let Err(returned) = sink.publish(batch).await {
                        reader.batch_returned(returned);
                    }
                }
            }
        }
        // Drain whatever is left, retrying until the sink lets go of it.
        for _ in 0..8 {
            let batch = reader.take_batch();
            if let Err(returned) = sink.publish(batch).await {
                reader.batch_returned(returned);
            }
        }

        let mut writer = HotRegistry::new(64);
        let now = Duration::from_secs(9);
        while let Ok(batch) = received.try_recv() {
            for model in &batch.families {
                writer.merge_relayed(model.family, model, batch.age_of(model), now);
            }
        }

        let relayed = writer
            .model(ObservationModelKind::Statement, &family_of(&alone))
            .expect("the relayed family");
        let served = alone
            .model(ObservationModelKind::Statement, &family_of(&alone))
            .expect("the observed family");
        assert_eq!(
            relayed.executions, served.executions,
            "the writer's view of the reader's workload is not what the reader saw"
        );
        assert_eq!(relayed.rows.count(), served.rows.count());
        assert_eq!(relayed.rows.maximum(), served.rows.maximum());
        for quantile in [0.5, 0.95, 1.0] {
            assert_eq!(
                relayed.rows.quantile_upper_bound(quantile),
                served.rows.quantile_upper_bound(quantile),
                "row distribution differs at quantile {quantile}"
            );
        }
        assert_eq!(relayed.plans, served.plans);
        assert_eq!(relayed.q_error_max_x100, served.q_error_max_x100);
    }

    fn family_of(registry: &HotRegistry) -> Fingerprint {
        registry
            .models()
            .next()
            .expect("one observed family")
            .family
    }

    /// Every instance can receive, so the runner always exposes the seam.
    #[tokio::test]
    async fn a_runner_folds_a_relayed_batch_into_its_published_models() {
        let runner = StatisticsRunner::start(
            Arc::new(crate::runtime::SystemRuntime),
            super::super::statistics::StatisticsConfig {
                publish_interval: Duration::from_millis(10),
                ..Default::default()
            },
            None,
            None,
        );
        let outcome = runner.ingest().submit(ObservationBatch {
            format: RELAY_FORMAT,
            instance: "reader-1".into(),
            boot: "boot".into(),
            sequence: 1,
            sent_at_micros: 0,
            families: vec![model(1, 6)],
            frequency: vec![(family(2), 4)],
            corpus: Vec::new(),
        });
        assert_eq!(outcome, IngestOutcome::Accepted);

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let stats = runner.stats();
            if let Some(model) = stats.statement_models.get(&family(1)) {
                assert_eq!(model.retained_executions, 6);
                assert!(stats.frequency(&family(2)) >= 4);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "relayed evidence never reached the published models"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        runner.shutdown().await;
    }
}
