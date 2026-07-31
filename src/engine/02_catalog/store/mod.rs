mod durable_json;
mod definitions;
mod fences;
mod mode;
mod reclamations;
mod retention;
mod revision_compaction;
mod revisions;
mod tables;
mod transition_cleanup;
mod transition_compaction;
mod transition_violations;
mod transitions;
mod write_protocol_canonical;

pub use definitions::*;
pub use fences::*;
pub use mode::*;
pub use reclamations::*;
pub use retention::*;
pub use revision_compaction::*;
pub use revisions::*;
pub use tables::*;
pub use transition_cleanup::*;
pub use transition_compaction::*;
pub use transition_violations::*;
pub use transitions::*;

use crate::engine::catalog::{Error, ErrorKind, Result};
use crate::engine::kv;
use crate::engine::kv::KeyRange;
use crate::engine::kv::key_encoding::prefix_end;

pub(crate) fn prefix_bounds(start: Vec<u8>) -> (Vec<u8>, Vec<u8>) {
    let end = prefix_end(&start).expect("catalog prefix has an upper bound");
    (start, end)
}

pub(crate) fn prefix_range(prefix: &[u8]) -> KeyRange {
    let (start, end) = prefix_bounds(prefix.to_vec());
    KeyRange::new(start, end)
}

pub(crate) fn parse_u64(kind: &str, id: Option<&str>, raw: &[u8]) -> Result<u64> {
    let identity = id.map_or_else(String::new, |id| format!(" for {id:?}"));
    let message = || format!("catalog: corrupt {kind}{identity} {raw:?}");
    let value = std::str::from_utf8(raw)
        .map_err(|error| Error::source(ErrorKind::CatalogCorrupt, message(), error))?;
    value
        .parse::<u64>()
        .map_err(|error| Error::source(ErrorKind::CatalogCorrupt, message(), error))
}

pub(crate) fn map_kv(error: kv::Error) -> Error {
    error.into()
}
