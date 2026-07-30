//! Versioned schema and catalog state.

mod error;
mod change;
mod facade;
pub mod identity;
pub mod migrate;
pub mod model;
pub mod naming;
pub mod schema;
pub mod store;

pub use change::{Mutation, Service};
pub use error::{Error, ErrorKind, Result};
pub use facade::Catalog;
