//! OpenAPI-backed HTTP transport for the transport-neutral Rad engine.

#[allow(
    clippy::collapsible_if,
    clippy::double_must_use,
    clippy::nonminimal_bool
)]
#[rustfmt::skip]
pub mod generated;

mod administration;
mod catalog;
mod cors;
mod listener;
mod meta;
mod problem;
mod result;
mod schema;
mod server;
mod validation;
mod wire;

pub use listener::serve;
pub use server::{Api, router, router_with_location};

#[cfg(test)]
mod tests;
