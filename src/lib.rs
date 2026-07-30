pub mod admin;
pub mod cli;
pub mod codegen;
pub mod engine;
pub mod http;
pub mod protocol;
pub mod process;
pub mod runtime;
pub mod scheduler;
pub mod service;

/// Run Rad from its environment configuration until an operating-system
/// shutdown signal arrives.
pub async fn run() -> process::Result {
    process::run().await
}
