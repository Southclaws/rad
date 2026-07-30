//! Catalog binding, logical analysis, and physical query planning.

mod error;
mod dependencies;
mod plan;
pub mod analysis;
pub mod bind;
pub mod explain;
pub mod physical;

#[cfg(test)]
pub(crate) mod test_support;

pub use error::{Error, ErrorClass, Reason, Result};
pub use plan::{PlanOptions, plan_query};
