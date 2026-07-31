//! Physical plan execution.

pub mod codec;
mod engine;
mod error;
mod events;
mod frames;
mod pipeline;
mod program;
mod query;
mod reference;
mod row_store;
mod set;
pub mod schema_jobs;
mod mutate;
mod write;

pub use engine::Engine;
pub use error::{Error, ErrorKind, ErrorReason, Result};
pub use events::{EngineEvent, EngineEventHook, EngineOperation, GateAction, NoopEngineEventHook};
pub use frames::shape_frames;
pub(crate) use program::resolve_default;
pub use program::{
    CatalogExpectation, CatalogPolicy, DefaultSpec, Program, ProgramOptions, ProgramResult,
    Statement, StatementPlan, StatementResult,
};
pub use query::{Executor, Limits};
pub use reference::ReferenceExecutor;
