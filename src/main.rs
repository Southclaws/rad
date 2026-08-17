use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    Box::pin(rad::cli::run()).await
}
