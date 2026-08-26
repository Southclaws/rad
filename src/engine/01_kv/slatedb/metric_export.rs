use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use opentelemetry::metrics::{
    Counter as OtelCounter, Gauge as OtelGauge, Histogram as OtelHistogram,
    UpDownCounter as OtelUpDownCounter,
};
use opentelemetry::{KeyValue, global};

use super::slate_db;

pub(super) struct WriteAmplification {
    wal_bytes: AtomicU64,
    l0_bytes: AtomicU64,
    compacted_bytes: AtomicU64,
    memtable_bytes: AtomicU64,
    gauge: Option<OtelGauge<f64>>,
}

impl WriteAmplification {
    pub(super) fn new() -> Arc<Self> {
        let gauge = crate::telemetry::enabled().then(|| {
            global::meter("rad")
                .f64_gauge("rad.storage.write.amplification")
                .with_description("Rad storage write amplification")
                .build()
        });
        Arc::new(Self {
            wal_bytes: AtomicU64::new(0),
            l0_bytes: AtomicU64::new(0),
            compacted_bytes: AtomicU64::new(0),
            memtable_bytes: AtomicU64::new(0),
            gauge,
        })
    }

    fn add(&self, input: WriteAmplificationInput, value: u64) {
        let target = match input {
            WriteAmplificationInput::Wal => &self.wal_bytes,
            WriteAmplificationInput::L0 => &self.l0_bytes,
            WriteAmplificationInput::Compacted => &self.compacted_bytes,
            WriteAmplificationInput::Memtable => &self.memtable_bytes,
        };
        target.fetch_add(value, Ordering::Relaxed);
        let Some(gauge) = &self.gauge else {
            return;
        };
        if let Some(amplification) = self.ratio() {
            gauge.record(amplification, &[]);
        }
    }

    fn ratio(&self) -> Option<f64> {
        let memtable = self.memtable_bytes.load(Ordering::Relaxed);
        (memtable != 0).then(|| {
            let written = self
                .wal_bytes
                .load(Ordering::Relaxed)
                .saturating_add(self.l0_bytes.load(Ordering::Relaxed))
                .saturating_add(self.compacted_bytes.load(Ordering::Relaxed));
            written as f64 / memtable as f64
        })
    }
}

#[derive(Clone, Copy)]
enum WriteAmplificationInput {
    Wal,
    L0,
    Compacted,
    Memtable,
}

pub(super) struct Counter {
    instrument: OtelCounter<u64>,
    attributes: Vec<KeyValue>,
    write_amplification: Option<(Arc<WriteAmplification>, WriteAmplificationInput)>,
}

impl Counter {
    pub(super) fn new(
        name: &str,
        labels: &[(&str, &str)],
        write_amplification: &Arc<WriteAmplification>,
    ) -> Option<Self> {
        if !crate::telemetry::enabled() {
            return None;
        }
        let definition = counter_definition(name, labels)?;
        let instrument = global::meter("rad")
            .u64_counter(definition.name)
            .with_description(definition.description)
            .build();
        Some(Self {
            instrument,
            attributes: definition.attributes,
            write_amplification: definition
                .write_amplification
                .map(|input| (Arc::clone(write_amplification), input)),
        })
    }

    pub(super) fn add(&self, value: u64) {
        self.instrument.add(value, &self.attributes);
        if let Some((state, input)) = &self.write_amplification {
            state.add(*input, value);
        }
    }
}

pub(super) struct Gauge {
    instrument: OtelGauge<i64>,
    attributes: Vec<KeyValue>,
}

impl Gauge {
    pub(super) fn new(name: &str, labels: &[(&str, &str)]) -> Option<Self> {
        if !crate::telemetry::enabled() {
            return None;
        }
        let definition = gauge_definition(name, labels)?;
        Some(Self {
            instrument: global::meter("rad")
                .i64_gauge(definition.name)
                .with_description(definition.description)
                .build(),
            attributes: definition.attributes,
        })
    }

    pub(super) fn record(&self, value: i64) {
        self.instrument.record(value, &self.attributes);
    }
}

pub(super) struct UpDownCounter {
    instrument: OtelUpDownCounter<i64>,
    attributes: Vec<KeyValue>,
}

impl UpDownCounter {
    pub(super) fn new(name: &str, labels: &[(&str, &str)]) -> Option<Self> {
        if !crate::telemetry::enabled() {
            return None;
        }
        let definition = up_down_counter_definition(name, labels)?;
        Some(Self {
            instrument: global::meter("rad")
                .i64_up_down_counter(definition.name)
                .with_description(definition.description)
                .build(),
            attributes: definition.attributes,
        })
    }

    pub(super) fn add(&self, value: i64) {
        self.instrument.add(value, &self.attributes);
    }
}

pub(super) struct Histogram {
    instrument: OtelHistogram<f64>,
    attributes: Vec<KeyValue>,
}

impl Histogram {
    pub(super) fn new(name: &str, labels: &[(&str, &str)]) -> Option<Self> {
        if !crate::telemetry::enabled() {
            return None;
        }
        let definition = histogram_definition(name, labels)?;
        Some(Self {
            instrument: global::meter("rad")
                .f64_histogram(definition.name)
                .with_unit("s")
                .with_description(definition.description)
                .build(),
            attributes: definition.attributes,
        })
    }

    pub(super) fn record(&self, value: f64) {
        self.instrument.record(value, &self.attributes);
    }
}

struct Definition {
    name: &'static str,
    description: &'static str,
    attributes: Vec<KeyValue>,
    write_amplification: Option<WriteAmplificationInput>,
}

fn counter_definition(name: &str, labels: &[(&str, &str)]) -> Option<Definition> {
    let mut attributes = Vec::new();
    let (name, description, write_amplification) = match name {
        slate_db::db_stats::REQUEST_COUNT => {
            copy_label(labels, "op", "rad.storage.operation", &mut attributes);
            (
                "rad.storage.database.requests",
                "Slate database requests",
                None,
            )
        }
        slate_db::db_stats::WRITE_OPS => (
            "rad.storage.write.operations",
            "Slate database write operations",
            None,
        ),
        slate_db::db_stats::WRITE_BATCH_COUNT => (
            "rad.storage.write.batches",
            "Slate database write batches",
            None,
        ),
        slate_db::db_stats::BACKPRESSURE_COUNT => (
            "rad.storage.backpressure.events",
            "Slate database backpressure events",
            None,
        ),
        slate_db::db_stats::L0_STALL_COUNT => {
            copy_label(
                labels,
                "type",
                "rad.storage.lsm.stall.reason",
                &mut attributes,
            );
            ("rad.storage.lsm.stalls", "Slate L0 stall events", None)
        }
        slate_db::db_stats::IMMUTABLE_MEMTABLE_FLUSHES => (
            "rad.storage.memtable.flushes",
            "Slate immutable memory-table flushes",
            None,
        ),
        slate_db::db_stats::L0_FLUSH_BYTES => (
            "rad.storage.lsm.flush.bytes",
            "Bytes flushed to Slate L0 storage",
            Some(WriteAmplificationInput::L0),
        ),
        slate_db::db_stats::MEMTABLE_WRITE_BYTES => (
            "rad.storage.memtable.write.bytes",
            "Bytes written to the Slate memory table",
            Some(WriteAmplificationInput::Memtable),
        ),
        slate_db::db_stats::SST_FILTER_FALSE_POSITIVE_COUNT => {
            copy_label(labels, "kind", "rad.storage.filter.kind", &mut attributes);
            attributes.push(KeyValue::new("rad.storage.filter.result", "false_positive"));
            ("rad.storage.filter.checks", "Slate SST filter checks", None)
        }
        slate_db::db_stats::SST_FILTER_POSITIVE_COUNT => {
            copy_label(labels, "kind", "rad.storage.filter.kind", &mut attributes);
            attributes.push(KeyValue::new("rad.storage.filter.result", "positive"));
            ("rad.storage.filter.checks", "Slate SST filter checks", None)
        }
        slate_db::db_stats::SST_FILTER_NEGATIVE_COUNT => {
            copy_label(labels, "kind", "rad.storage.filter.kind", &mut attributes);
            attributes.push(KeyValue::new("rad.storage.filter.result", "negative"));
            ("rad.storage.filter.checks", "Slate SST filter checks", None)
        }
        slate_db::db_stats::MERGE_OPERATOR_OPERANDS => {
            copy_label(labels, "path", "rad.storage.merge.path", &mut attributes);
            ("rad.storage.merge.operands", "Slate merge operands", None)
        }
        slate_db::db_cache_stats::ACCESS_COUNT => {
            attributes.push(KeyValue::new("rad.storage.cache.tier", "decoded"));
            copy_label(
                labels,
                "entry_kind",
                "rad.storage.cache.entry_kind",
                &mut attributes,
            );
            copy_label(
                labels,
                "result",
                "rad.storage.cache.result",
                &mut attributes,
            );
            (
                "rad.storage.slate.cache.accesses",
                "Slate cache accesses",
                None,
            )
        }
        slate_db::db_cache_stats::ERROR_COUNT => {
            attributes.push(KeyValue::new("rad.storage.cache.tier", "decoded"));
            attributes.push(KeyValue::new("rad.storage.cache.result", "error"));
            (
                "rad.storage.slate.cache.accesses",
                "Slate cache accesses",
                None,
            )
        }
        slate_db::cached_object_store_stats::PART_ACCESS_COUNT => {
            attributes.push(KeyValue::new("rad.storage.cache.tier", "local_object"));
            attributes.push(KeyValue::new("rad.storage.cache.result", "access"));
            (
                "rad.storage.slate.cache.accesses",
                "Slate cache accesses",
                None,
            )
        }
        slate_db::cached_object_store_stats::PART_HIT_COUNT => {
            attributes.push(KeyValue::new("rad.storage.cache.tier", "local_object"));
            attributes.push(KeyValue::new("rad.storage.cache.result", "hit"));
            (
                "rad.storage.slate.cache.accesses",
                "Slate cache accesses",
                None,
            )
        }
        slate_db::cached_object_store_stats::EVICTED_KEYS => (
            "rad.storage.cache.evictions",
            "Slate local cache evicted keys",
            None,
        ),
        slate_db::cached_object_store_stats::EVICTED_BYTES => (
            "rad.storage.cache.evicted.bytes",
            "Slate local cache evicted bytes",
            None,
        ),
        slate_db::instrumented_object_store_stats::REQUEST_COUNT => {
            object_store_attributes(labels, &mut attributes);
            (
                "rad.storage.object.requests",
                "Slate object-store requests",
                None,
            )
        }
        slate_db::instrumented_object_store_stats::ERROR_COUNT => {
            object_store_attributes(labels, &mut attributes);
            (
                "rad.storage.object.unsuccessful.requests",
                "Slate object-store unsuccessful requests, including control flow",
                None,
            )
        }
        slate_db::wal_buffer_stats::WAL_BUFFER_FLUSHES => (
            "rad.storage.wal.buffer.flushes",
            "Slate WAL buffer flushes",
            None,
        ),
        slate_db::wal_buffer_stats::WAL_BUFFER_FLUSH_REQUESTS => (
            "rad.storage.wal.buffer.flush.requests",
            "Slate WAL buffer flush requests",
            None,
        ),
        slate_db::wal_buffer_stats::WAL_FLUSH_BYTES => (
            "rad.storage.wal.flush.bytes",
            "Bytes flushed to Slate WAL storage",
            Some(WriteAmplificationInput::Wal),
        ),
        slate_db::compactor::stats::BYTES_COMPACTED => (
            "rad.storage.compaction.bytes",
            "Bytes compacted by Slate",
            Some(WriteAmplificationInput::Compacted),
        ),
        slate_db::compactor::stats::SSTS_WRITTEN => (
            "rad.storage.compaction.ssts",
            "SSTs written by Slate compaction",
            None,
        ),
        slate_db::compactor::stats::JOBS_CLAIMED => (
            "rad.storage.compaction.jobs.claimed",
            "Slate compaction jobs claimed",
            None,
        ),
        slate_db::compactor::stats::JOBS_RECLAIMED => (
            "rad.storage.compaction.jobs.reclaimed",
            "Slate compaction jobs reclaimed",
            None,
        ),
        slate_db::compactor::stats::EXPIRED_ENTRIES_PURGED => {
            copy_label(
                labels,
                "entry_type",
                "rad.storage.entry.type",
                &mut attributes,
            );
            (
                "rad.storage.compaction.expired.entries",
                "Expired entries purged by Slate compaction",
                None,
            )
        }
        slate_db::garbage_collector_stats::GC_COUNT => {
            ("rad.storage.gc.runs", "Slate garbage-collection runs", None)
        }
        slate_db::garbage_collector_stats::DELETED_COUNT => {
            copy_label(
                labels,
                "resource",
                "rad.storage.gc.resource",
                &mut attributes,
            );
            (
                "rad.storage.gc.deleted.objects",
                "Objects deleted by Slate garbage collection",
                None,
            )
        }
        _ => return None,
    };
    Some(Definition {
        name,
        description,
        attributes,
        write_amplification,
    })
}

fn gauge_definition(name: &str, _labels: &[(&str, &str)]) -> Option<Definition> {
    let mut attributes = Vec::new();
    let (name, description) = match name {
        slate_db::db_stats::TOTAL_MEM_SIZE_BYTES => (
            "rad.storage.memory.bytes",
            "Bytes retained in Slate memory tables",
        ),
        slate_db::db_stats::L0_SST_COUNT => {
            attributes.push(KeyValue::new("rad.storage.lsm.level", "l0"));
            ("rad.storage.lsm.ssts", "Slate SST count")
        }
        slate_db::db_stats::SEGMENT_MAX_L0_SST_COUNT => (
            "rad.storage.lsm.segment.max_l0_ssts",
            "Maximum Slate L0 SST count for one segment",
        ),
        slate_db::db_stats::SORTED_RUN_COUNT => {
            ("rad.storage.lsm.sorted_runs", "Slate sorted-run count")
        }
        slate_db::db_stats::SST_VIEW_COUNT => ("rad.storage.lsm.sst_views", "Slate SST view count"),
        slate_db::db_stats::SST_COUNT => {
            attributes.push(KeyValue::new("rad.storage.lsm.level", "all"));
            ("rad.storage.lsm.ssts", "Slate SST count")
        }
        slate_db::db_stats::EXTERNAL_DB_COUNT => (
            "rad.storage.external.databases",
            "Slate external database count",
        ),
        slate_db::cached_object_store_stats::CACHE_KEYS => (
            "rad.storage.cache.keys",
            "Keys retained in the Slate local cache",
        ),
        slate_db::cached_object_store_stats::CACHE_BYTES => (
            "rad.storage.cache.size.bytes",
            "Bytes retained in the Slate local cache",
        ),
        slate_db::wal_buffer_stats::WAL_BUFFER_ESTIMATED_BYTES => (
            "rad.storage.wal.buffer.bytes",
            "Estimated bytes in the Slate WAL buffer",
        ),
        slate_db::compactor::stats::COMPACTOR_EPOCH => {
            ("rad.storage.compaction.epoch", "Slate compactor epoch")
        }
        slate_db::compactor::stats::LAST_COMPACTION_TS_SEC => (
            "rad.storage.compaction.last.timestamp",
            "Unix timestamp of the last Slate compaction",
        ),
        slate_db::compactor::stats::TOTAL_BYTES_BEING_COMPACTED => (
            "rad.storage.compaction.backlog.bytes",
            "Bytes in active Slate compactions",
        ),
        slate_db::compactor::stats::TOTAL_THROUGHPUT_BYTES_PER_SEC => (
            "rad.storage.compaction.throughput",
            "Slate compaction throughput in bytes per second",
        ),
        _ => return None,
    };
    Some(Definition {
        name,
        description,
        attributes,
        write_amplification: None,
    })
}

fn up_down_counter_definition(name: &str, _labels: &[(&str, &str)]) -> Option<Definition> {
    let (name, description) = match name {
        slate_db::compactor::stats::RUNNING_COMPACTIONS => {
            ("rad.storage.compaction.active", "Active Slate compactions")
        }
        _ => return None,
    };
    Some(Definition {
        name,
        description,
        attributes: Vec::new(),
        write_amplification: None,
    })
}

fn histogram_definition(name: &str, labels: &[(&str, &str)]) -> Option<Definition> {
    if name != slate_db::instrumented_object_store_stats::REQUEST_DURATION_SECONDS {
        return None;
    }
    let mut attributes = Vec::new();
    object_store_attributes(labels, &mut attributes);
    Some(Definition {
        name: "rad.storage.object.request.duration",
        description: "Slate object-store request duration",
        attributes,
        write_amplification: None,
    })
}

fn object_store_attributes(labels: &[(&str, &str)], attributes: &mut Vec<KeyValue>) {
    copy_label(labels, "component", "rad.storage.component", attributes);
    copy_label(labels, "store_type", "rad.storage.store", attributes);
    copy_label(labels, "op", "rad.storage.operation", attributes);
    copy_label(labels, "api", "rad.storage.api", attributes);
    if let Some(component) = label(labels, "component") {
        let activity = match component {
            "db" | "reader" => "foreground",
            "compactor" | "gc" => "background",
            _ => "unknown",
        };
        attributes.push(KeyValue::new("rad.storage.activity", activity));
    }
}

fn copy_label(
    labels: &[(&str, &str)],
    source: &str,
    target: &'static str,
    attributes: &mut Vec<KeyValue>,
) {
    if let Some(value) = label(labels, source) {
        attributes.push(KeyValue::new(target, value.to_owned()));
    }
}

fn label<'a>(labels: &'a [(&str, &str)], key: &str) -> Option<&'a str> {
    labels
        .iter()
        .find_map(|(label, value)| (*label == key).then_some(*value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_store_metrics_keep_bounded_activity_attributes() {
        let definition = counter_definition(
            slate_db::instrumented_object_store_stats::REQUEST_COUNT,
            &[
                ("component", "compactor"),
                ("store_type", "main"),
                ("op", "get"),
                ("api", "get_range"),
                ("worker_id", "unbounded"),
            ],
        )
        .unwrap();
        assert_eq!(definition.name, "rad.storage.object.requests");
        assert!(definition.attributes.iter().any(|attribute| {
            attribute.key.as_str() == "rad.storage.activity"
                && attribute.value.as_str() == "background"
        }));
        assert!(
            definition
                .attributes
                .iter()
                .all(|attribute| attribute.key.as_str() != "worker_id")
        );
    }

    #[test]
    fn write_amplification_inputs_are_complete() {
        for metric in [
            slate_db::wal_buffer_stats::WAL_FLUSH_BYTES,
            slate_db::db_stats::L0_FLUSH_BYTES,
            slate_db::compactor::stats::BYTES_COMPACTED,
            slate_db::db_stats::MEMTABLE_WRITE_BYTES,
        ] {
            assert!(
                counter_definition(metric, &[])
                    .unwrap()
                    .write_amplification
                    .is_some()
            );
        }
    }

    #[test]
    fn write_amplification_uses_all_physical_write_bytes() {
        let state = WriteAmplification::new();
        state.add(WriteAmplificationInput::Wal, 100);
        state.add(WriteAmplificationInput::L0, 200);
        state.add(WriteAmplificationInput::Compacted, 300);
        assert_eq!(state.ratio(), None);
        state.add(WriteAmplificationInput::Memtable, 150);
        assert_eq!(state.ratio(), Some(4.0));
    }
}
