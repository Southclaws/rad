use std::sync::OnceLock;
use std::time::Duration;

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use opentelemetry::metrics::{
    Counter, Gauge, Histogram, ObservableCounter, ObservableGauge, UpDownCounter,
};
use opentelemetry::{KeyValue, global};
use opentelemetry_otlp::{Protocol, WithExportConfig as _};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::metrics::{Aggregation, Instrument, InstrumentKind, Stream};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::resource::EnvResourceDetector;
use opentelemetry_sdk::trace::SdkTracerProvider;
use prometheus::{Encoder as _, Registry, TextEncoder};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

pub(crate) struct Runtime {
    tracer_provider: Option<SdkTracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
    prometheus_registry: Option<Registry>,
}

impl Runtime {
    pub(crate) fn build(
        config: &Config,
    ) -> Result<Option<Self>, Box<dyn std::error::Error + Send + Sync>> {
        if config.endpoint.is_none() && !config.metrics {
            return Ok(None);
        }
        let resource = resource(config);
        let tracer_provider = config
            .endpoint
            .as_deref()
            .map(|endpoint| {
                let exporter = opentelemetry_otlp::SpanExporter::builder()
                    .with_http()
                    .with_protocol(Protocol::HttpBinary)
                    .with_endpoint(signal_endpoint(endpoint, "v1/traces"))
                    .build()?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(
                    SdkTracerProvider::builder()
                        .with_resource(resource.clone())
                        .with_batch_exporter(exporter)
                        .build(),
                )
            })
            .transpose()?;
        let (meter_provider, prometheus_registry) = if config.metrics {
            let registry = Registry::new();
            let prometheus = opentelemetry_prometheus::exporter()
                .with_registry(registry.clone())
                .build()?;
            let mut builder = SdkMeterProvider::builder()
                .with_resource(resource)
                .with_reader(prometheus)
                .with_view(rad_metric_view);
            if let Some(endpoint) = config.endpoint.as_deref() {
                let exporter = opentelemetry_otlp::MetricExporter::builder()
                    .with_http()
                    .with_protocol(Protocol::HttpBinary)
                    .with_endpoint(signal_endpoint(endpoint, "v1/metrics"))
                    .build()?;
                builder = builder.with_periodic_exporter(exporter);
            }
            (Some(builder.build()), Some(registry))
        } else {
            (None, None)
        };
        Ok(Some(Self {
            tracer_provider,
            meter_provider,
            prometheus_registry,
        }))
    }

    pub(crate) fn tracer(&self) -> Option<opentelemetry_sdk::trace::SdkTracer> {
        use opentelemetry::trace::TracerProvider as _;
        self.tracer_provider
            .as_ref()
            .map(|provider| provider.tracer("rad"))
    }

    pub(crate) fn activate(&self) {
        global::set_text_map_propagator(TraceContextPropagator::new());
        if let Some(provider) = &self.meter_provider {
            global::set_meter_provider(provider.clone());
            let _ = INSTRUMENTS.set(Instruments::new());
        }
        if let Some(registry) = &self.prometheus_registry {
            let _ = PROMETHEUS_REGISTRY.set(registry.clone());
        }
    }

    pub(crate) fn shutdown(&self, timeout: Duration) {
        if let Some(provider) = &self.meter_provider {
            let _ = provider.shutdown_with_timeout(timeout);
        }
        if let Some(provider) = &self.tracer_provider {
            let _ = provider.shutdown_with_timeout(timeout);
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub endpoint: Option<String>,
    pub instance_id: Option<String>,
    pub role: Option<String>,
    pub metrics: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            endpoint: None,
            instance_id: None,
            role: None,
            metrics: true,
        }
    }
}

fn resource(config: &Config) -> Resource {
    let mut attributes = vec![
        KeyValue::new("service.name", "rad"),
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
    ];
    if let Some(instance_id) = &config.instance_id {
        attributes.push(KeyValue::new("service.instance.id", instance_id.clone()));
    }
    if let Some(role) = &config.role {
        attributes.push(KeyValue::new("rad.role", role.clone()));
    }
    for (environment, attribute) in [
        (
            "OTEL_RESOURCE_ATTRIBUTES_K8S_NAMESPACE_NAME",
            "k8s.namespace.name",
        ),
        ("OTEL_RESOURCE_ATTRIBUTES_K8S_POD_NAME", "k8s.pod.name"),
        ("OTEL_RESOURCE_ATTRIBUTES_RAD_DATABASE", "rad.database.name"),
    ] {
        if let Ok(value) = std::env::var(environment)
            && !value.is_empty()
        {
            attributes.push(KeyValue::new(attribute, value));
        }
    }
    Resource::builder()
        .with_detector(Box::new(EnvResourceDetector::new()))
        .with_attributes(attributes)
        .build()
}

fn signal_endpoint(endpoint: &str, signal_path: &str) -> String {
    if endpoint.ends_with(signal_path) {
        endpoint.to_owned()
    } else {
        format!("{}/{signal_path}", endpoint.trim_end_matches('/'))
    }
}

fn rad_metric_view(instrument: &Instrument) -> Option<Stream> {
    let name = instrument.name();
    if !name.starts_with("rad.") && !name.starts_with("http.server.") {
        return None;
    }
    let mut stream = Stream::builder().with_cardinality_limit(256);
    if instrument.kind() == InstrumentKind::Histogram {
        let boundaries = match name {
            name if name.ends_with("duration") => Some(vec![
                0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
                60.0, 120.0,
            ]),
            "rad.relation.cache.cohort.reuse.opportunities" => Some(vec![
                0.0, 1.0, 2.0, 3.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0, 1024.0,
            ]),
            "rad.relation.cache.shadow.candidate.work" => Some(vec![
                4_096.0,
                16_384.0,
                65_536.0,
                262_144.0,
                1_048_576.0,
                4_194_304.0,
                16_777_216.0,
                67_108_864.0,
                268_435_456.0,
                1_073_741_824.0,
            ]),
            "rad.relation.cache.shadow.candidate.density" => Some(vec![
                0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 256.0, 1024.0,
            ]),
            "rad.relation.cache.result.bytes" | "rad.relation.cache.prepared.candidate.bytes" => {
                Some(vec![
                    1_024.0,
                    4_096.0,
                    16_384.0,
                    65_536.0,
                    262_144.0,
                    1_048_576.0,
                    4_194_304.0,
                    8_388_608.0,
                    16_777_216.0,
                    67_108_864.0,
                    134_217_728.0,
                ])
            }
            _ => None,
        };
        if let Some(boundaries) = boundaries {
            stream = stream.with_aggregation(Aggregation::ExplicitBucketHistogram {
                boundaries,
                record_min_max: true,
            });
        }
    }
    stream.build().ok()
}

struct Instruments {
    _process_cpu_time: ObservableCounter<f64>,
    _process_cpu_limit: ObservableGauge<f64>,
    _process_cpu_throttled_time: ObservableCounter<f64>,
    _process_cpu_throttled_events: ObservableCounter<u64>,
    _process_memory_rss: ObservableGauge<u64>,
    _process_memory_limit: ObservableGauge<u64>,
    http_active: UpDownCounter<i64>,
    http_requests: Counter<u64>,
    http_duration: Histogram<f64>,
    program_active: UpDownCounter<i64>,
    program_admitted_active: UpDownCounter<i64>,
    program_queue_active: UpDownCounter<i64>,
    program_queue_duration: Histogram<f64>,
    program_executions: Counter<u64>,
    program_duration: Histogram<f64>,
    program_statements: Histogram<u64>,
    program_result_rows: Histogram<u64>,
    program_affected_rows: Histogram<u64>,
    statement_executions: Counter<u64>,
    statement_duration: Histogram<f64>,
    statement_rows: Counter<u64>,
    planner_duration: Histogram<f64>,
    operator_duration: Histogram<f64>,
    operator_rows: Histogram<u64>,
    execution_parallel_width: Histogram<u64>,
    execution_parallel_batches: Counter<u64>,
    execution_parallel_rows: Counter<u64>,
    relation_cache_lookups: Counter<u64>,
    relation_cache_admissions: Counter<u64>,
    relation_cache_evictions: Counter<u64>,
    relation_cache_entries: Gauge<u64>,
    relation_cache_retained_bytes: Gauge<u64>,
    relation_cache_capacity_entries: Gauge<u64>,
    relation_cache_capacity_bytes: Gauge<u64>,
    relation_cache_result_limit_bytes: Gauge<u64>,
    relation_cache_result_bytes: Histogram<u64>,
    relation_cache_dependency_lookups: Counter<u64>,
    relation_cache_dependency_evictions: Counter<u64>,
    relation_cache_catalog_lookups: Counter<u64>,
    relation_cache_catalog_evictions: Counter<u64>,
    relation_cache_catalog_entries: Gauge<u64>,
    relation_cache_catalog_retained_bytes: Gauge<u64>,
    relation_cache_prepared_lookups: Counter<u64>,
    relation_cache_prepared_admissions: Counter<u64>,
    relation_cache_prepared_evictions: Counter<u64>,
    relation_cache_prepared_entries: Gauge<u64>,
    relation_cache_prepared_retained_bytes: Gauge<u64>,
    relation_cache_prepared_capacity_entries: Gauge<u64>,
    relation_cache_prepared_capacity_bytes: Gauge<u64>,
    relation_cache_prepared_plan_limit_bytes: Gauge<u64>,
    relation_cache_prepared_candidate_bytes: Histogram<u64>,
    relation_cache_prepared_avoided_binds: Counter<u64>,
    relation_cache_prepared_avoided_plans: Counter<u64>,
    relation_cache_avoided_reads: Counter<u64>,
    relation_cache_avoided_bytes: Counter<u64>,
    relation_cache_avoided_time: Counter<f64>,
    relation_cache_coalesced_fills: Counter<u64>,
    relation_cache_reuse_opportunities: Counter<u64>,
    relation_cache_cohort_transitions: Counter<u64>,
    relation_cache_cohort_reuse: Histogram<u64>,
    relation_cache_shadow_decisions: Counter<u64>,
    relation_cache_shadow_candidate_work: Histogram<u64>,
    relation_cache_shadow_candidate_density: Histogram<f64>,
    relation_cache_shadow_rejected_reuse: Counter<u64>,
    relation_cache_evidence_evictions: Counter<u64>,
    kv_operations: Counter<u64>,
    kv_operation_duration: Histogram<f64>,
    kv_bytes: Counter<u64>,
    kv_scan_rows: Histogram<u64>,
    transaction_completions: Counter<u64>,
    transaction_duration: Histogram<f64>,
    transaction_conflicts: Counter<u64>,
    postgres_active: UpDownCounter<i64>,
    postgres_connections: Counter<u64>,
    storage_available: Gauge<i64>,
    storage_outages: Counter<u64>,
    storage_outage_duration: Histogram<f64>,
    storage_fencing: Counter<u64>,
    storage_cache_accesses: Counter<u64>,
    storage_cache_capacity: Gauge<i64>,
    storage_requests: Counter<u64>,
    storage_request_duration: Histogram<f64>,
    storage_request_bytes: Counter<u64>,
    scheduler_jobs: Counter<u64>,
    scheduler_quarantined: Counter<u64>,
    scheduler_lost_workers: Counter<u64>,
}

impl Instruments {
    fn new() -> Self {
        let meter = global::meter("rad");
        Self {
            _process_cpu_time: meter
                .f64_observable_counter("rad.process.cpu.time")
                .with_unit("s")
                .with_description("Rad process CPU time")
                .with_callback(|observer| {
                    if let Some(value) = cgroup_stat("usage_usec") {
                        observer.observe(value as f64 / 1_000_000.0, &[]);
                    }
                })
                .build(),
            _process_cpu_limit: meter
                .f64_observable_gauge("rad.process.cpu.limit")
                .with_unit("{cpu}")
                .with_description("Rad process CPU limit")
                .with_callback(|observer| {
                    if let Some(value) = process_cpu_limit() {
                        observer.observe(value, &[]);
                    }
                })
                .build(),
            _process_cpu_throttled_time: meter
                .f64_observable_counter("rad.process.cpu.throttled.duration")
                .with_unit("s")
                .with_description("Rad process CPU throttled duration")
                .with_callback(|observer| {
                    if let Some(value) = cgroup_stat("throttled_usec") {
                        observer.observe(value as f64 / 1_000_000.0, &[]);
                    }
                })
                .build(),
            _process_cpu_throttled_events: meter
                .u64_observable_counter("rad.process.cpu.throttled.events")
                .with_description("Rad process CPU throttled events")
                .with_callback(|observer| {
                    if let Some(value) = cgroup_stat("nr_throttled") {
                        observer.observe(value, &[]);
                    }
                })
                .build(),
            _process_memory_rss: meter
                .u64_observable_gauge("rad.process.memory.rss")
                .with_unit("By")
                .with_description("Rad process resident memory")
                .with_callback(|observer| {
                    if let Some(value) = process_rss_bytes() {
                        observer.observe(value, &[]);
                    }
                })
                .build(),
            _process_memory_limit: meter
                .u64_observable_gauge("rad.process.memory.limit")
                .with_unit("By")
                .with_description("Rad process memory limit")
                .with_callback(|observer| {
                    if let Some(value) = cgroup_memory_limit() {
                        observer.observe(value, &[]);
                    }
                })
                .build(),
            http_active: meter
                .i64_up_down_counter("http.server.active_requests")
                .with_description("Active Rad HTTP requests")
                .build(),
            http_requests: meter
                .u64_counter("http.server.requests")
                .with_description("Completed Rad HTTP requests")
                .build(),
            http_duration: meter
                .f64_histogram("http.server.request.duration")
                .with_unit("s")
                .with_description("Rad HTTP request duration")
                .build(),
            program_active: meter
                .i64_up_down_counter("rad.program.active")
                .with_description("Active Rad programs")
                .build(),
            program_admitted_active: meter
                .i64_up_down_counter("rad.program.admitted.active")
                .with_description("Programs admitted for execution")
                .build(),
            program_queue_active: meter
                .i64_up_down_counter("rad.program.queue.active")
                .with_description("Programs waiting for execution")
                .build(),
            program_queue_duration: meter
                .f64_histogram("rad.program.queue.duration")
                .with_unit("s")
                .with_description("Program execution queue duration")
                .build(),
            program_executions: meter
                .u64_counter("rad.program.executions")
                .with_description("Completed Rad programs")
                .build(),
            program_duration: meter
                .f64_histogram("rad.program.duration")
                .with_unit("s")
                .with_description("Rad program duration")
                .build(),
            program_result_rows: meter
                .u64_histogram("rad.program.result.rows")
                .with_description("Rows returned by Rad programs")
                .build(),
            program_affected_rows: meter
                .u64_histogram("rad.program.affected.rows")
                .with_description("Rows changed by Rad programs")
                .build(),
            program_statements: meter
                .u64_histogram("rad.program.statements")
                .with_description("Statements in a Rad program")
                .build(),
            statement_executions: meter
                .u64_counter("rad.statement.executions")
                .with_description("Completed Rad statements")
                .build(),
            statement_duration: meter
                .f64_histogram("rad.statement.duration")
                .with_unit("s")
                .with_description("Rad statement duration")
                .build(),
            statement_rows: meter
                .u64_counter("rad.statement.rows")
                .with_description("Rows produced by Rad statements")
                .build(),
            planner_duration: meter
                .f64_histogram("rad.planner.duration")
                .with_unit("s")
                .with_description("Rad statement binding and planning duration")
                .build(),
            operator_duration: meter
                .f64_histogram("rad.operator.duration")
                .with_unit("s")
                .with_description("Rad physical operator duration")
                .build(),
            operator_rows: meter
                .u64_histogram("rad.operator.rows")
                .with_description("Rows processed by Rad physical operators")
                .build(),
            execution_parallel_width: meter
                .u64_histogram("rad.execution.parallel.width")
                .with_description("Workers used by a parallel execution batch")
                .build(),
            execution_parallel_batches: meter
                .u64_counter("rad.execution.parallel.batches")
                .with_description("Parallel execution batches")
                .build(),
            execution_parallel_rows: meter
                .u64_counter("rad.execution.parallel.rows")
                .with_description("Rows processed by parallel execution batches")
                .build(),
            relation_cache_lookups: meter
                .u64_counter("rad.relation.cache.lookups")
                .with_description("Rad relation cache lookups")
                .build(),
            relation_cache_admissions: meter
                .u64_counter("rad.relation.cache.admissions")
                .with_description("Rad relation cache admission results")
                .build(),
            relation_cache_evictions: meter
                .u64_counter("rad.relation.cache.evictions")
                .with_description("Rad relation cache removals")
                .build(),
            relation_cache_entries: meter
                .u64_gauge("rad.relation.cache.entries")
                .with_description("Rad relation cache entries")
                .build(),
            relation_cache_retained_bytes: meter
                .u64_gauge("rad.relation.cache.retained.bytes")
                .with_unit("By")
                .with_description("Rad relation cache retained bytes")
                .build(),
            relation_cache_capacity_entries: meter
                .u64_gauge("rad.relation.cache.capacity.entries")
                .with_description("Rad relation cache entry limit")
                .build(),
            relation_cache_capacity_bytes: meter
                .u64_gauge("rad.relation.cache.capacity.bytes")
                .with_unit("By")
                .with_description("Rad relation cache byte limit")
                .build(),
            relation_cache_result_limit_bytes: meter
                .u64_gauge("rad.relation.cache.result.limit.bytes")
                .with_unit("By")
                .with_description("Rad relation cache individual result byte limit")
                .build(),
            relation_cache_result_bytes: meter
                .u64_histogram("rad.relation.cache.result.bytes")
                .with_unit("By")
                .with_description("Relation cache candidate result bytes")
                .build(),
            relation_cache_dependency_lookups: meter
                .u64_counter("rad.relation.cache.dependency.lookups")
                .with_description("Relation cache dependency validation lookups")
                .build(),
            relation_cache_dependency_evictions: meter
                .u64_counter("rad.relation.cache.dependency.evictions")
                .with_description("Entries removed from snapshot dependency validation")
                .build(),
            relation_cache_catalog_lookups: meter
                .u64_counter("rad.relation.cache.catalog.lookups")
                .with_description("Snapshot catalog metadata cache lookups")
                .build(),
            relation_cache_catalog_evictions: meter
                .u64_counter("rad.relation.cache.catalog.evictions")
                .with_description("Entries removed from the snapshot catalog metadata cache")
                .build(),
            relation_cache_catalog_entries: meter
                .u64_gauge("rad.relation.cache.catalog.entries")
                .with_description("Snapshot catalog metadata cache entries")
                .build(),
            relation_cache_catalog_retained_bytes: meter
                .u64_gauge("rad.relation.cache.catalog.retained.bytes")
                .with_unit("By")
                .with_description("Snapshot catalog metadata cache retained bytes")
                .build(),
            relation_cache_prepared_lookups: meter
                .u64_counter("rad.relation.cache.prepared.lookups")
                .with_description("Prepared read cache lookups")
                .build(),
            relation_cache_prepared_admissions: meter
                .u64_counter("rad.relation.cache.prepared.admissions")
                .with_description("Prepared read cache admission results")
                .build(),
            relation_cache_prepared_evictions: meter
                .u64_counter("rad.relation.cache.prepared.evictions")
                .with_description("Entries removed from the prepared read cache")
                .build(),
            relation_cache_prepared_entries: meter
                .u64_gauge("rad.relation.cache.prepared.entries")
                .with_description("Prepared read cache entries")
                .build(),
            relation_cache_prepared_retained_bytes: meter
                .u64_gauge("rad.relation.cache.prepared.retained.bytes")
                .with_unit("By")
                .with_description("Prepared read cache retained bytes")
                .build(),
            relation_cache_prepared_capacity_entries: meter
                .u64_gauge("rad.relation.cache.prepared.capacity.entries")
                .with_description("Prepared read cache entry limit")
                .build(),
            relation_cache_prepared_capacity_bytes: meter
                .u64_gauge("rad.relation.cache.prepared.capacity.bytes")
                .with_unit("By")
                .with_description("Prepared read cache byte limit")
                .build(),
            relation_cache_prepared_plan_limit_bytes: meter
                .u64_gauge("rad.relation.cache.prepared.plan.limit.bytes")
                .with_unit("By")
                .with_description("Prepared read cache individual plan byte limit")
                .build(),
            relation_cache_prepared_candidate_bytes: meter
                .u64_histogram("rad.relation.cache.prepared.candidate.bytes")
                .with_unit("By")
                .with_description("Prepared read cache candidate bytes")
                .build(),
            relation_cache_prepared_avoided_binds: meter
                .u64_counter("rad.relation.cache.prepared.avoided.binds")
                .with_description("Query binds avoided by the prepared read cache")
                .build(),
            relation_cache_prepared_avoided_plans: meter
                .u64_counter("rad.relation.cache.prepared.avoided.plans")
                .with_description("Physical plans avoided by the prepared read cache")
                .build(),
            relation_cache_avoided_reads: meter
                .u64_counter("rad.relation.cache.avoided.reads")
                .with_description("Logical reads avoided by the Rad relation cache")
                .build(),
            relation_cache_avoided_bytes: meter
                .u64_counter("rad.relation.cache.avoided.bytes")
                .with_unit("By")
                .with_description("Logical read bytes avoided by the Rad relation cache")
                .build(),
            relation_cache_avoided_time: meter
                .f64_counter("rad.relation.cache.avoided.time")
                .with_unit("s")
                .with_description("Execution time avoided by the Rad relation cache")
                .build(),
            relation_cache_coalesced_fills: meter
                .u64_counter("rad.relation.cache.coalesced.fills")
                .with_description("Rad relation cache fills shared by concurrent requests")
                .build(),
            relation_cache_reuse_opportunities: meter
                .u64_counter("rad.relation.cache.reuse.opportunities")
                .with_description(
                    "Requests after the first request for one exact dependency cohort",
                )
                .build(),
            relation_cache_cohort_transitions: meter
                .u64_counter("rad.relation.cache.cohort.transitions")
                .with_description("Observed relation cache dependency cohort transitions")
                .build(),
            relation_cache_cohort_reuse: meter
                .u64_histogram("rad.relation.cache.cohort.reuse.opportunities")
                .with_description(
                    "Reuse opportunities observed before a dependency cohort is superseded",
                )
                .build(),
            relation_cache_shadow_decisions: meter
                .u64_counter("rad.relation.cache.shadow.decisions")
                .with_description(
                    "Relation cache admission decisions from the inactive policy scorer",
                )
                .build(),
            relation_cache_shadow_candidate_work: meter
                .u64_histogram("rad.relation.cache.shadow.candidate.work")
                .with_description(
                    "Deterministic work units for relation cache admission candidates",
                )
                .build(),
            relation_cache_shadow_candidate_density: meter
                .f64_histogram("rad.relation.cache.shadow.candidate.density")
                .with_description("Deterministic work units per retained candidate byte")
                .build(),
            relation_cache_shadow_rejected_reuse: meter
                .u64_counter("rad.relation.cache.shadow.rejected.reuse")
                .with_description("Reuse opportunities after the shadow policy rejects a candidate")
                .build(),
            relation_cache_evidence_evictions: meter
                .u64_counter("rad.relation.cache.evidence.evictions")
                .with_description("Entries removed from bounded relation cache policy evidence")
                .build(),
            kv_operations: meter
                .u64_counter("rad.kv.operations")
                .with_description("Rad logical KV operations")
                .build(),
            kv_bytes: meter
                .u64_counter("rad.kv.io.bytes")
                .with_description("Rad logical KV bytes")
                .build(),
            kv_operation_duration: meter
                .f64_histogram("rad.kv.operation.duration")
                .with_unit("s")
                .with_description("Rad logical KV operation duration")
                .build(),
            kv_scan_rows: meter
                .u64_histogram("rad.kv.scan.rows")
                .with_description("Rows returned by Rad logical KV scans")
                .build(),
            transaction_completions: meter
                .u64_counter("rad.transaction.completions")
                .with_description("Completed Rad transactions")
                .build(),
            transaction_duration: meter
                .f64_histogram("rad.transaction.duration")
                .with_unit("s")
                .with_description("Rad transaction duration")
                .build(),
            transaction_conflicts: meter
                .u64_counter("rad.transaction.conflicts")
                .with_description("Rad transaction conflicts")
                .build(),
            postgres_active: meter
                .i64_up_down_counter("rad.postgresql.connections.active")
                .with_description("Active PostgreSQL connections")
                .build(),
            postgres_connections: meter
                .u64_counter("rad.postgresql.connections")
                .with_description("Completed PostgreSQL connections")
                .build(),
            storage_available: meter
                .i64_gauge("rad.storage.available")
                .with_description("Rad storage availability")
                .build(),
            storage_outages: meter
                .u64_counter("rad.storage.outages")
                .with_description("Rad storage outages")
                .build(),
            storage_outage_duration: meter
                .f64_histogram("rad.storage.outage.duration")
                .with_unit("s")
                .with_description("Rad storage outage duration")
                .build(),
            storage_fencing: meter
                .u64_counter("rad.storage.fencing")
                .with_description("Rad storage fencing events")
                .build(),
            storage_cache_accesses: meter
                .u64_counter("rad.storage.cache.accesses")
                .with_description("Rad statement storage cache accesses")
                .build(),
            storage_cache_capacity: meter
                .i64_gauge("rad.storage.cache.capacity.bytes")
                .with_description("Configured Rad storage cache capacity")
                .build(),
            storage_requests: meter
                .u64_counter("rad.storage.requests")
                .with_description("Rad statement backing storage requests")
                .build(),
            storage_request_duration: meter
                .f64_histogram("rad.storage.request.duration")
                .with_unit("s")
                .with_description("Rad statement backing storage request duration")
                .build(),
            storage_request_bytes: meter
                .u64_counter("rad.storage.request.bytes")
                .with_description("Rad statement backing storage bytes")
                .build(),
            scheduler_jobs: meter
                .u64_counter("rad.scheduler.jobs")
                .with_description("Rad scheduler job batches")
                .build(),
            scheduler_quarantined: meter
                .u64_counter("rad.scheduler.quarantined")
                .with_description("Rad scheduler quarantined jobs")
                .build(),
            scheduler_lost_workers: meter
                .u64_counter("rad.scheduler.lost_workers")
                .with_description("Rad scheduler lost workers")
                .build(),
        }
    }
}

#[cfg(target_os = "linux")]
fn cgroup_stat(name: &str) -> Option<u64> {
    let contents = std::fs::read_to_string("/sys/fs/cgroup/cpu.stat").ok()?;
    contents.lines().find_map(|line| {
        let (key, value) = line.split_once(' ')?;
        (key == name).then(|| value.parse().ok()).flatten()
    })
}

#[cfg(not(target_os = "linux"))]
fn cgroup_stat(_name: &str) -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
pub(crate) fn process_cpu_limit() -> Option<f64> {
    let contents = std::fs::read_to_string("/sys/fs/cgroup/cpu.max").ok()?;
    let (quota, period) = contents.trim().split_once(' ')?;
    if quota == "max" {
        return None;
    }
    Some(quota.parse::<f64>().ok()? / period.parse::<f64>().ok()?)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn process_cpu_limit() -> Option<f64> {
    None
}

#[cfg(target_os = "linux")]
fn cgroup_memory_limit() -> Option<u64> {
    let value = std::fs::read_to_string("/sys/fs/cgroup/memory.max").ok()?;
    (value.trim() != "max")
        .then(|| value.trim().parse().ok())
        .flatten()
}

#[cfg(not(target_os = "linux"))]
fn cgroup_memory_limit() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn process_rss_bytes() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = contents.lines().find(|line| line.starts_with("VmRSS:"))?;
    let kibibytes = line.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    kibibytes.checked_mul(1024)
}

#[cfg(not(target_os = "linux"))]
fn process_rss_bytes() -> Option<u64> {
    None
}

static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();
static PROMETHEUS_REGISTRY: OnceLock<Registry> = OnceLock::new();

pub fn enabled() -> bool {
    INSTRUMENTS.get().is_some()
}

pub(crate) async fn prometheus_metrics() -> Response {
    let Some(registry) = PROMETHEUS_REGISTRY.get() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match encode_prometheus(registry) {
        Ok(body) => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            body,
        )
            .into_response(),
        Err(()) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn encode_prometheus(registry: &Registry) -> Result<Vec<u8>, ()> {
    let mut body = Vec::new();
    TextEncoder::new()
        .encode(&registry.gather(), &mut body)
        .map_err(|_| ())?;
    Ok(body)
}

pub fn span_ids(span: &tracing::Span) -> (String, String) {
    use opentelemetry::trace::TraceContextExt as _;

    let context = span.context();
    let span = context.span();
    let context = span.span_context();
    if !context.is_valid() {
        return (String::new(), String::new());
    }
    (
        context.trace_id().to_string(),
        context.span_id().to_string(),
    )
}

pub fn http_started(method: &str) {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments.http_active.add(
            1,
            &[KeyValue::new("http.request.method", method.to_owned())],
        );
    }
}

pub fn http_finished(method: &str, status: u16, duration: Duration) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    let attributes = [
        KeyValue::new("http.request.method", method.to_owned()),
        KeyValue::new("http.response.status_code", i64::from(status)),
    ];
    instruments.http_active.add(-1, &attributes[..1]);
    instruments.http_requests.add(1, &attributes);
    instruments
        .http_duration
        .record(duration.as_secs_f64(), &attributes);
}

pub fn program_started(transport: &str) {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments
            .program_active
            .add(1, &[KeyValue::new("rad.transport", transport.to_owned())]);
    }
}

pub fn program_admitted_started() {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments.program_admitted_active.add(1, &[]);
    }
}

pub fn program_admitted_finished() {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments.program_admitted_active.add(-1, &[]);
    }
}

pub fn program_queue_started() {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments.program_queue_active.add(1, &[]);
    }
}

pub fn program_queue_finished(duration: Duration) {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments.program_queue_active.add(-1, &[]);
        instruments
            .program_queue_duration
            .record(duration.as_secs_f64(), &[]);
    }
}

pub fn program_queue_cancelled() {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments.program_queue_active.add(-1, &[]);
    }
}

pub struct ProgramMeasurement<'a> {
    pub transport: &'a str,
    pub status: &'a str,
    pub duration: Duration,
    pub result_rows: u64,
    pub affected_rows: u64,
    pub statements: u64,
    pub dry_run: bool,
}

pub fn program_finished(measurement: ProgramMeasurement<'_>) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    let active_attributes = [KeyValue::new(
        "rad.transport",
        measurement.transport.to_owned(),
    )];
    let attributes = [
        active_attributes[0].clone(),
        KeyValue::new("rad.status", measurement.status.to_owned()),
        KeyValue::new("rad.program.dry_run", measurement.dry_run),
    ];
    instruments.program_active.add(-1, &active_attributes);
    instruments.program_executions.add(1, &attributes);
    instruments
        .program_duration
        .record(measurement.duration.as_secs_f64(), &attributes);
    instruments
        .program_result_rows
        .record(measurement.result_rows, &attributes);
    instruments
        .program_affected_rows
        .record(measurement.affected_rows, &attributes);
    instruments
        .program_statements
        .record(measurement.statements, &active_attributes);
}

pub fn statement_finished(
    kind: &str,
    source: crate::engine::exec::observe::StatementSource,
    status: &str,
    planning: Duration,
    execution: Duration,
    rows: u64,
    kv: &crate::engine::exec::observe::KvWork,
) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    let attributes = [
        KeyValue::new("rad.statement.kind", kind.to_owned()),
        KeyValue::new("rad.statement.source", source.as_str()),
        KeyValue::new("rad.status", status.to_owned()),
    ];
    instruments.statement_executions.add(1, &attributes);
    instruments
        .statement_duration
        .record(execution.as_secs_f64(), &attributes);
    instruments.statement_rows.add(rows, &attributes);
    instruments
        .planner_duration
        .record(planning.as_secs_f64(), &attributes);
    let _ = kv;
}

pub fn operator_finished(measurement: &crate::engine::exec::observe::OperatorMeasurement) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    let operator = KeyValue::new("rad.operator.name", measurement.operator);
    instruments.operator_duration.record(
        measurement.exclusive_micros as f64 / 1_000_000.0,
        &[
            operator.clone(),
            KeyValue::new("rad.operator.measurement", "exclusive"),
        ],
    );
    instruments.operator_duration.record(
        measurement.open_micros as f64 / 1_000_000.0,
        &[
            operator.clone(),
            KeyValue::new("rad.operator.measurement", "open"),
        ],
    );
    instruments.operator_rows.record(
        measurement.input_rows,
        &[
            operator.clone(),
            KeyValue::new("rad.operator.direction", "input"),
        ],
    );
    instruments.operator_rows.record(
        measurement.output_rows,
        &[operator, KeyValue::new("rad.operator.direction", "output")],
    );
}

pub fn execution_parallel_batch(operator: &str, width: usize, rows: usize) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    let attributes = [KeyValue::new("rad.operator.name", operator.to_owned())];
    instruments
        .execution_parallel_width
        .record(width as u64, &attributes);
    instruments.execution_parallel_batches.add(1, &attributes);
    instruments
        .execution_parallel_rows
        .add(rows as u64, &attributes);
}

pub fn relation_cache_lookup(result: &'static str) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments
        .relation_cache_lookups
        .add(1, &[KeyValue::new("rad.cache.result", result)]);
}

pub fn relation_cache_admission(result: &'static str) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments
        .relation_cache_admissions
        .add(1, &[KeyValue::new("rad.cache.result", result)]);
}

pub fn relation_cache_eviction(cause: &'static str) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments
        .relation_cache_evictions
        .add(1, &[KeyValue::new("rad.cache.cause", cause)]);
}

pub fn relation_cache_residency(entries: usize, retained_bytes: u64) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments
        .relation_cache_entries
        .record(entries as u64, &[]);
    instruments
        .relation_cache_retained_bytes
        .record(retained_bytes, &[]);
}

pub fn relation_cache_limits(entries: usize, bytes: u64, result_bytes: u64) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments
        .relation_cache_capacity_entries
        .record(entries as u64, &[]);
    instruments.relation_cache_capacity_bytes.record(bytes, &[]);
    instruments
        .relation_cache_result_limit_bytes
        .record(result_bytes, &[]);
}

pub fn relation_cache_result_size(bytes: usize) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments
        .relation_cache_result_bytes
        .record(bytes as u64, &[]);
}

pub fn relation_cache_dependency_lookup(result: &'static str) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments
        .relation_cache_dependency_lookups
        .add(1, &[KeyValue::new("rad.cache.result", result)]);
}

pub fn relation_cache_dependency_eviction(count: u64) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments
        .relation_cache_dependency_evictions
        .add(count, &[]);
}

pub fn relation_cache_catalog_lookup(result: &'static str) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments
        .relation_cache_catalog_lookups
        .add(1, &[KeyValue::new("rad.cache.result", result)]);
}

pub fn relation_cache_catalog_eviction(count: u64) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments.relation_cache_catalog_evictions.add(count, &[]);
}

pub fn relation_cache_catalog_residency(entries: u64, retained_bytes: u64) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments
        .relation_cache_catalog_entries
        .record(entries, &[]);
    instruments
        .relation_cache_catalog_retained_bytes
        .record(retained_bytes, &[]);
}

pub fn relation_cache_prepared_lookup(result: &'static str) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments
        .relation_cache_prepared_lookups
        .add(1, &[KeyValue::new("rad.cache.result", result)]);
}

pub fn relation_cache_prepared_admission(result: &'static str) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments
        .relation_cache_prepared_admissions
        .add(1, &[KeyValue::new("rad.cache.result", result)]);
}

pub fn relation_cache_prepared_eviction(count: u64) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments
        .relation_cache_prepared_evictions
        .add(count, &[]);
}

pub fn relation_cache_prepared_residency(entries: u64, retained_bytes: u64) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments
        .relation_cache_prepared_entries
        .record(entries, &[]);
    instruments
        .relation_cache_prepared_retained_bytes
        .record(retained_bytes, &[]);
}

pub fn relation_cache_prepared_limits(entries: u64, bytes: u64, plan_bytes: u64) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments
        .relation_cache_prepared_capacity_entries
        .record(entries, &[]);
    instruments
        .relation_cache_prepared_capacity_bytes
        .record(bytes, &[]);
    instruments
        .relation_cache_prepared_plan_limit_bytes
        .record(plan_bytes, &[]);
}

pub fn relation_cache_prepared_candidate(bytes: u64) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments
        .relation_cache_prepared_candidate_bytes
        .record(bytes, &[]);
}

pub fn relation_cache_prepared_avoided() {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments
        .relation_cache_prepared_avoided_binds
        .add(1, &[]);
    instruments
        .relation_cache_prepared_avoided_plans
        .add(1, &[]);
}

pub fn relation_cache_avoided(reads: u64, bytes: u64, duration: Duration) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments.relation_cache_avoided_reads.add(reads, &[]);
    instruments.relation_cache_avoided_bytes.add(bytes, &[]);
    instruments
        .relation_cache_avoided_time
        .add(duration.as_secs_f64(), &[]);
}

pub fn relation_cache_coalesced() {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments.relation_cache_coalesced_fills.add(1, &[]);
}

pub fn relation_cache_reuse_opportunity() {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments.relation_cache_reuse_opportunities.add(1, &[]);
}

pub fn relation_cache_cohort_transition(cause: &'static str) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments
        .relation_cache_cohort_transitions
        .add(1, &[KeyValue::new("rad.cache.cause", cause)]);
}

pub fn relation_cache_cohort_reuse(reuse_opportunities: u64, outcome: &'static str) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments.relation_cache_cohort_reuse.record(
        reuse_opportunities,
        &[KeyValue::new("rad.cache.cohort.outcome", outcome)],
    );
}

pub fn relation_cache_shadow_decision(
    decision: &'static str,
    reason: &'static str,
    evidence: &'static str,
) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments.relation_cache_shadow_decisions.add(
        1,
        &[
            KeyValue::new("rad.cache.decision", decision),
            KeyValue::new("rad.cache.reason", reason),
            KeyValue::new("rad.cache.evidence", evidence),
        ],
    );
}

pub fn relation_cache_shadow_candidate(work_units: u64, density: f64) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments
        .relation_cache_shadow_candidate_work
        .record(work_units, &[]);
    instruments
        .relation_cache_shadow_candidate_density
        .record(density, &[]);
}

pub fn relation_cache_shadow_rejected_reuse() {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments.relation_cache_shadow_rejected_reuse.add(1, &[]);
}

pub fn relation_cache_evidence_eviction(cause: &'static str) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments
        .relation_cache_evidence_evictions
        .add(1, &[KeyValue::new("rad.cache.cause", cause)]);
}

pub fn storage_cache_observed(
    tier: &str,
    entry_kind: Option<&str>,
    accesses: u64,
    hits: u64,
    misses: u64,
    errors: u64,
) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    let record = |count, result| {
        if count == 0 {
            return;
        }
        instruments.storage_cache_accesses.add(
            count,
            &[
                KeyValue::new("rad.storage.cache.tier", tier.to_owned()),
                KeyValue::new(
                    "rad.storage.cache.entry_kind",
                    entry_kind.unwrap_or("all").to_owned(),
                ),
                KeyValue::new("rad.storage.cache.result", result),
            ],
        );
    };
    record(accesses, "access");
    record(hits, "hit");
    record(misses, "miss");
    record(errors, "error");
}

pub fn storage_cache_capacity(entry_kind: &str, bytes: u64) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    instruments.storage_cache_capacity.record(
        i64::try_from(bytes).unwrap_or(i64::MAX),
        &[
            KeyValue::new("rad.storage.cache.tier", "memory"),
            KeyValue::new("rad.storage.cache.entry_kind", entry_kind.to_owned()),
        ],
    );
}

pub fn storage_request_finished(
    service_tier: &str,
    class: &str,
    completed: bool,
    error: bool,
    bytes: u64,
    duration: Duration,
) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    let status = if error {
        "error"
    } else if completed {
        "success"
    } else {
        "incomplete"
    };
    let attributes = [
        KeyValue::new("rad.storage.service_tier", service_tier.to_owned()),
        KeyValue::new("rad.storage.request.class", class.to_owned()),
        KeyValue::new("rad.status", status),
    ];
    instruments.storage_requests.add(1, &attributes);
    instruments
        .storage_request_duration
        .record(duration.as_secs_f64(), &attributes);
    if bytes > 0 {
        instruments.storage_request_bytes.add(bytes, &attributes);
    }
}

pub struct KvMeasurement<'a> {
    pub operation: &'a str,
    pub keyspace: &'a str,
    pub status: &'a str,
    pub duration: Duration,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub rows: Option<u64>,
}

pub fn kv_finished(measurement: KvMeasurement<'_>) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    let attributes = [
        KeyValue::new("rad.kv.operation", measurement.operation.to_owned()),
        KeyValue::new("rad.kv.keyspace", measurement.keyspace.to_owned()),
        KeyValue::new("rad.status", measurement.status.to_owned()),
    ];
    instruments.kv_operations.add(1, &attributes);
    instruments
        .kv_operation_duration
        .record(measurement.duration.as_secs_f64(), &attributes);
    if measurement.bytes_read > 0 {
        instruments.kv_bytes.add(
            measurement.bytes_read,
            &[
                KeyValue::new("rad.kv.direction", "read"),
                KeyValue::new("rad.kv.keyspace", measurement.keyspace.to_owned()),
            ],
        );
    }
    if measurement.bytes_written > 0 {
        instruments.kv_bytes.add(
            measurement.bytes_written,
            &[
                KeyValue::new("rad.kv.direction", "write"),
                KeyValue::new("rad.kv.keyspace", measurement.keyspace.to_owned()),
            ],
        );
    }
    if let Some(rows) = measurement.rows {
        instruments.kv_scan_rows.record(rows, &attributes);
    }
}

pub fn transaction_finished(
    outcome: &str,
    status: &str,
    duration: Duration,
    conflict: Option<&str>,
) {
    let Some(instruments) = INSTRUMENTS.get() else {
        return;
    };
    let attributes = [
        KeyValue::new("rad.transaction.outcome", outcome.to_owned()),
        KeyValue::new("rad.status", status.to_owned()),
    ];
    instruments.transaction_completions.add(1, &attributes);
    instruments
        .transaction_duration
        .record(duration.as_secs_f64(), &attributes);
    if let Some(reason) = conflict {
        instruments
            .transaction_conflicts
            .add(1, &[KeyValue::new("rad.error.reason", reason.to_owned())]);
    }
}

pub fn postgres_connection_started() {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments.postgres_active.add(1, &[]);
    }
}

pub fn postgres_connection_finished(outcome: &str) {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments.postgres_active.add(-1, &[]);
        instruments
            .postgres_connections
            .add(1, &[KeyValue::new("rad.outcome", outcome.to_owned())]);
    }
}

pub fn storage_observed(available: bool) {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments
            .storage_available
            .record(i64::from(available), &[]);
    }
}

pub fn storage_outage_started(reason: &str) {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments
            .storage_outages
            .add(1, &[KeyValue::new("rad.error.reason", reason.to_owned())]);
    }
}

pub fn storage_outage_finished(reason: &str, duration: Duration) {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments.storage_outage_duration.record(
            duration.as_secs_f64(),
            &[KeyValue::new("rad.error.reason", reason.to_owned())],
        );
    }
}

pub fn storage_fenced(reason: &str) {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments
            .storage_fencing
            .add(1, &[KeyValue::new("rad.error.reason", reason.to_owned())]);
    }
}

pub fn scheduler_job(kind: &str, outcome: &str) {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments.scheduler_jobs.add(
            1,
            &[
                KeyValue::new("rad.scheduler.job.kind", kind.to_owned()),
                KeyValue::new("rad.outcome", outcome.to_owned()),
            ],
        );
    }
}

pub fn scheduler_quarantined(kind: &str, reason: &str) {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments.scheduler_quarantined.add(
            1,
            &[
                KeyValue::new("rad.scheduler.job.kind", kind.to_owned()),
                KeyValue::new("rad.error.reason", reason.to_owned()),
            ],
        );
    }
}

pub fn scheduler_worker_lost() {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments.scheduler_lost_workers.add(1, &[]);
    }
}

#[cfg(test)]
mod tests {
    use super::signal_endpoint;

    #[test]
    fn signal_endpoint_accepts_base_and_signal_endpoints() {
        assert_eq!(
            signal_endpoint("http://collector:4318", "v1/traces"),
            "http://collector:4318/v1/traces"
        );
        assert_eq!(
            signal_endpoint("http://collector:4318/v1/traces", "v1/traces"),
            "http://collector:4318/v1/traces"
        );
    }
}
