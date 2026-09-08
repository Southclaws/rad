//! Physical plan execution.

pub mod codec;
pub mod diagnostic;
mod engine;
pub mod observe;
pub mod survey;
mod error;
mod events;
mod frames;
pub mod key_describe;
mod pipeline;
mod predicate_transfer;
mod program;
mod query;
mod reference;
mod relation_cache;
mod row_store;
mod shredded_join;
mod set;
pub mod schema_jobs;
mod mutate;
mod parallel;
mod write;

pub use engine::Engine;
pub use error::{Error, ErrorKind, ErrorReason, Result};
pub use events::{EngineEvent, EngineEventHook, EngineOperation, GateAction, NoopEngineEventHook};
pub use frames::shape_frames;
pub(crate) use program::resolve_default;
pub use program::{
    CatalogExpectation, CatalogPolicy, DefaultSpec, PreparedStatementEstimate, Program,
    ProgramOptions, ProgramResult, Statement, StatementPlan, StatementPlanMeasurement,
    StatementResult,
};
pub use query::{Executor, Limits};
pub use reference::ReferenceExecutor;
pub use relation_cache::RelationCacheLimits;
