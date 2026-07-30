//! Process-local orchestration over durable engine work.
//!
//! Schedulers choose when bounded engine kernels run. They do not own catalog
//! correctness state and are deliberately separate from transports and the
//! numbered engine layers.

pub mod schema_jobs;
