//! OpenAPI-backed HTTP transport for the transport-neutral Rad engine.

#[allow(
    clippy::collapsible_if,
    clippy::double_must_use,
    clippy::nonminimal_bool,
    clippy::struct_excessive_bools,
    unused_qualifications
)]
#[rustfmt::skip]
pub mod generated;

mod administration;
mod catalog;
mod cors;
mod context;
mod listener;
mod meta;
mod probes;
mod problem;
mod result;
mod schema;
mod server;
mod validation;
mod wire;

pub use listener::serve;
pub use probes::router as probe_router;
pub use server::{Api, router, router_with_health, router_with_location};

#[cfg(test)]
mod tests;
