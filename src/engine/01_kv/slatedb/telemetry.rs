use std::collections::BTreeMap;
use std::sync::Arc;

use slatedb_common::metrics::{DefaultMetricsRecorder, Metric, MetricValue, MetricsRecorder};

use super::slate_db;
use crate::engine::kv::telemetry::{
    CumulativeHistogram, PHYSICAL_TELEMETRY_FORMAT, PhysicalCacheSnapshot, PhysicalCacheTier,
    PhysicalRequestClass, PhysicalRequestCondition, PhysicalRequestSnapshot, PhysicalTelemetry,
    PhysicalTelemetryCapabilities, PhysicalTelemetryIdentity, PhysicalTelemetrySnapshot,
};

pub(super) struct SlateTelemetry {
    recorder: Arc<DefaultMetricsRecorder>,
}

impl SlateTelemetry {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            recorder: Arc::new(DefaultMetricsRecorder::new()),
        })
    }

    pub(super) fn recorder(&self) -> Arc<dyn MetricsRecorder> {
        self.recorder.clone()
    }
}

#[derive(Default)]
struct RequestAccumulator {
    requests: u64,
    errors: u64,
    latency_micros: Option<CumulativeHistogram>,
}

impl PhysicalTelemetry for SlateTelemetry {
    fn snapshot(&self) -> PhysicalTelemetrySnapshot {
        let snapshot = self.recorder.snapshot();
        let mut requests = BTreeMap::<PhysicalRequestClass, RequestAccumulator>::new();
        for metric in snapshot.all() {
            let Some(class) = request_class(metric) else {
                continue;
            };
            let accumulated = requests.entry(class).or_default();
            match (metric.name.as_str(), &metric.value) {
                (
                    slate_db::instrumented_object_store_stats::REQUEST_COUNT,
                    MetricValue::Counter(value),
                ) => {
                    accumulated.requests = accumulated.requests.saturating_add(*value);
                }
                (
                    slate_db::instrumented_object_store_stats::ERROR_COUNT,
                    MetricValue::Counter(value),
                ) => {
                    accumulated.errors = accumulated.errors.saturating_add(*value);
                }
                (
                    slate_db::instrumented_object_store_stats::REQUEST_DURATION_SECONDS,
                    MetricValue::Histogram {
                        count,
                        max,
                        boundaries,
                        bucket_counts,
                        ..
                    },
                ) => merge_latency(
                    &mut accumulated.latency_micros,
                    *count,
                    *max,
                    boundaries,
                    bucket_counts,
                ),
                _ => {}
            }
        }

        let mut caches = Vec::new();
        let mut memory_accesses = 0u64;
        let mut memory_hits = 0u64;
        let mut local_accesses = None;
        let mut local_hits = None;
        for metric in snapshot.all() {
            match (metric.name.as_str(), &metric.value) {
                (slate_db::db_cache_stats::ACCESS_COUNT, MetricValue::Counter(value)) => {
                    memory_accesses = memory_accesses.saturating_add(*value);
                    if label(metric, "result") == Some("hit") {
                        memory_hits = memory_hits.saturating_add(*value);
                    }
                }
                (
                    slate_db::cached_object_store_stats::PART_ACCESS_COUNT,
                    MetricValue::Counter(value),
                ) => local_accesses = Some(*value),
                (
                    slate_db::cached_object_store_stats::PART_HIT_COUNT,
                    MetricValue::Counter(value),
                ) => local_hits = Some(*value),
                _ => {}
            }
        }
        if memory_accesses > 0 {
            caches.push(PhysicalCacheSnapshot {
                tier: PhysicalCacheTier::Memory,
                accesses: memory_accesses,
                hits: memory_hits,
            });
        }
        if let Some(accesses) = local_accesses {
            caches.push(PhysicalCacheSnapshot {
                tier: PhysicalCacheTier::Local,
                accesses,
                hits: local_hits.unwrap_or(0),
            });
        }

        PhysicalTelemetrySnapshot {
            identity: PhysicalTelemetryIdentity {
                backend: "slatedb".to_owned(),
                format: PHYSICAL_TELEMETRY_FORMAT,
            },
            capabilities: PhysicalTelemetryCapabilities {
                request_latency: true,
                cache_tiers: true,
                ..PhysicalTelemetryCapabilities::default()
            },
            requests: requests
                .into_iter()
                .map(|(class, accumulated)| PhysicalRequestSnapshot {
                    class,
                    condition: PhysicalRequestCondition::default(),
                    requests: accumulated.requests,
                    errors: accumulated.errors,
                    latency_micros: accumulated.latency_micros,
                    bytes: None,
                })
                .collect(),
            caches,
        }
    }
}

fn request_class(metric: &Metric) -> Option<PhysicalRequestClass> {
    if !matches!(label(metric, "component"), Some("db" | "reader"))
        || label(metric, "store_type") != Some("main")
    {
        return None;
    }
    match label(metric, "api")? {
        "get" => Some(PhysicalRequestClass::Read),
        "get_range" | "get_ranges" => Some(PhysicalRequestClass::RangeRead),
        "head" => Some(PhysicalRequestClass::MetadataRead),
        "put" | "multipart_init" | "multipart_part" | "multipart_complete" => {
            Some(PhysicalRequestClass::Write)
        }
        "delete" => Some(PhysicalRequestClass::Delete),
        "list" | "list_with_offset" | "list_with_delimiter" => Some(PhysicalRequestClass::List),
        _ => None,
    }
}

fn label<'a>(metric: &'a Metric, name: &str) -> Option<&'a str> {
    metric
        .labels
        .iter()
        .find_map(|(key, value)| (key == name).then_some(value.as_str()))
}

fn merge_latency(
    target: &mut Option<CumulativeHistogram>,
    count: u64,
    maximum_seconds: f64,
    boundaries_seconds: &[f64],
    bucket_counts: &[u64],
) {
    let boundaries = boundaries_seconds
        .iter()
        .map(|seconds| seconds_to_micros(*seconds))
        .collect::<Vec<_>>();
    let incoming = CumulativeHistogram {
        boundaries,
        bucket_counts: bucket_counts.to_vec(),
        count,
        maximum: seconds_to_micros(maximum_seconds),
    };
    let Some(current) = target else {
        *target = Some(incoming);
        return;
    };
    if current.boundaries != incoming.boundaries
        || current.bucket_counts.len() != incoming.bucket_counts.len()
    {
        return;
    }
    current.count = current.count.saturating_add(incoming.count);
    current.maximum = current.maximum.max(incoming.maximum);
    for (known, value) in current.bucket_counts.iter_mut().zip(incoming.bucket_counts) {
        *known = known.saturating_add(value);
    }
}

fn seconds_to_micros(seconds: f64) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }
    (seconds * 1_000_000.0).ceil().min(u64::MAX as f64) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slate_metrics_map_to_generic_request_and_cache_classes() {
        let telemetry = SlateTelemetry::new();
        let count = telemetry.recorder.register_counter(
            slate_db::instrumented_object_store_stats::REQUEST_COUNT,
            "",
            &[
                ("component", "reader"),
                ("store_type", "main"),
                ("op", "get"),
                ("api", "get_range"),
            ],
        );
        let latency = telemetry.recorder.register_histogram(
            slate_db::instrumented_object_store_stats::REQUEST_DURATION_SECONDS,
            "",
            &[
                ("component", "reader"),
                ("store_type", "main"),
                ("op", "get"),
                ("api", "get_range"),
            ],
            &[0.001, 0.01],
        );
        let cache = telemetry.recorder.register_counter(
            slate_db::db_cache_stats::ACCESS_COUNT,
            "",
            &[("entry_kind", "data_block"), ("result", "hit")],
        );
        count.increment(2);
        latency.record(0.002);
        latency.record(0.004);
        cache.increment(3);

        let snapshot = telemetry.snapshot();
        let request = snapshot
            .requests
            .iter()
            .find(|request| request.class == PhysicalRequestClass::RangeRead)
            .expect("range request");
        assert_eq!(request.requests, 2);
        assert_eq!(request.latency_micros.as_ref().expect("latency").count, 2);
        assert_eq!(snapshot.caches[0].tier, PhysicalCacheTier::Memory);
        assert_eq!(snapshot.caches[0].accesses, 3);
        assert_eq!(snapshot.caches[0].hits, 3);
    }

    #[test]
    fn background_and_wal_requests_do_not_enter_foreground_calibration() {
        let telemetry = SlateTelemetry::new();
        for labels in [
            [
                ("component", "compactor"),
                ("store_type", "main"),
                ("op", "get"),
                ("api", "get_range"),
            ],
            [
                ("component", "db"),
                ("store_type", "wal"),
                ("op", "get"),
                ("api", "get_range"),
            ],
        ] {
            telemetry
                .recorder
                .register_counter(
                    slate_db::instrumented_object_store_stats::REQUEST_COUNT,
                    "",
                    &labels,
                )
                .increment(1);
        }
        assert!(telemetry.snapshot().requests.is_empty());
    }
}
