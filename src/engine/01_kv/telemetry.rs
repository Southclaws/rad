//! Backend-neutral physical storage telemetry.

use std::sync::Arc;

pub const PHYSICAL_TELEMETRY_FORMAT: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhysicalTelemetryIdentity {
    pub backend: String,
    pub format: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct PhysicalTelemetryCapabilities {
    pub request_latency: bool,
    pub request_bytes: bool,
    pub request_concurrency: bool,
    pub cache_tiers: bool,
    pub access_locality: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PhysicalRequestClass {
    Read,
    RangeRead,
    MetadataRead,
    Write,
    Delete,
    List,
}

impl PhysicalRequestClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::RangeRead => "range_read",
            Self::MetadataRead => "metadata_read",
            Self::Write => "write",
            Self::Delete => "delete",
            Self::List => "list",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PhysicalCacheTier {
    Memory,
    Local,
}

impl PhysicalCacheTier {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Local => "local",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PhysicalServiceTier {
    Memory,
    Local,
    Remote,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhysicalRequestCondition {
    pub size_upper_bound: Option<u64>,
    pub concurrency_upper_bound: Option<u32>,
    pub service_tier: Option<PhysicalServiceTier>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CumulativeHistogram {
    pub boundaries: Vec<u64>,
    pub bucket_counts: Vec<u64>,
    pub count: u64,
    pub maximum: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PhysicalRequestSnapshot {
    pub class: PhysicalRequestClass,
    pub condition: PhysicalRequestCondition,
    pub requests: u64,
    pub errors: u64,
    pub latency_micros: Option<CumulativeHistogram>,
    pub bytes: Option<CumulativeHistogram>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PhysicalCacheSnapshot {
    pub tier: PhysicalCacheTier,
    pub accesses: u64,
    pub hits: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PhysicalTelemetrySnapshot {
    pub identity: PhysicalTelemetryIdentity,
    pub capabilities: PhysicalTelemetryCapabilities,
    pub requests: Vec<PhysicalRequestSnapshot>,
    pub caches: Vec<PhysicalCacheSnapshot>,
}

pub trait PhysicalTelemetry: Send + Sync {
    fn snapshot(&self) -> PhysicalTelemetrySnapshot;
}

pub type SharedPhysicalTelemetry = Arc<dyn PhysicalTelemetry>;
