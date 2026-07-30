//! Rad's canonical relational meaning layer.
//!
//! This module contains both the frontend-facing, name-based LIR and the
//! slot-addressed bound form trusted by planning and execution. It deliberately
//! contains no transaction, retry, scheduling, or transport state. Catalog
//! binding belongs to `engine::planner`, the next numbered layer.

mod types;
mod unbound;
mod value;

pub mod bound;
pub mod eval;
pub mod format;
pub mod inspect;

pub use types::*;
pub use unbound::*;
pub use value::*;
