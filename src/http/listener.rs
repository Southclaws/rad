use std::future::Future;
use std::io;

use axum::Router;
use tokio::net::TcpListener;

/// Serve a fully assembled Rad HTTP router until shutdown is requested.
///
/// Listener ownership stays with the caller so process configuration can bind
/// sockets before announcing readiness and tests can use an ephemeral port.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
}
