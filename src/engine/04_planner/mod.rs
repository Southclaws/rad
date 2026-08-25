//! Catalog binding, logical analysis, and physical query planning.

mod acyclic_join;
mod error;
mod dependencies;
mod join_estimator;
mod join_graph_estimator;
mod join_region;
mod join_search;
pub mod memo;
mod plan;
pub(crate) mod predicate_transfer;
pub mod analysis;
pub mod bind;
pub mod estimator;
pub mod explain;
pub mod models;
pub mod physical;

#[cfg(test)]
pub(crate) mod test_support;

pub use error::{Error, ErrorClass, Reason, Result};
pub use plan::{
    PlanOptions, PlannedQuery, PlannerMode, PlanningContext, plan_query, plan_query_with_context,
};
