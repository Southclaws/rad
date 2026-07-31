use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    rad::cli::run().await
}
