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

use crate::engine::catalog::{Error, ErrorKind};
use crate::engine::kv;

pub(crate) fn map_kv(error: kv::Error) -> Error {
    let kind = match error.kind() {
        kv::ErrorKind::Conflict => ErrorKind::Conflict,
        _ => ErrorKind::Storage,
    };
    Error::source(kind, format!("catalog storage: {error}"), error)
}
