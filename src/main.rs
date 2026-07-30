#[tokio::main]
async fn main() -> rad::process::Result {
    rad::cli::run().await
}
