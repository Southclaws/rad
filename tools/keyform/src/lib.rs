//! keyform: compiler for the normative storage format specification.
//!
//! `protocol/storage.keyform` declares the durable byte formats (ordered key
//! scalars, row bodies, the binary keyspace) together with the laws they
//! obey and authored byte vectors pinning them. This crate parses and
//! validates that specification, verifies the permanent allocation registry,
//! and emits the generated artifacts: the format manifest, the keyspace
//! registry, the typed key constructors and parsers, the key renderer, and
//! the conformance test suite.

pub mod allocations;
pub mod emit;
pub mod model;
pub mod parse;
pub mod schema;
pub mod validate;

pub use emit::{emit, spec_sha256};
pub use parse::parse;
pub use validate::validate;
