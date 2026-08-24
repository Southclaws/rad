//! Serving the internal router, with or without TLS.

use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use tokio::net::TcpListener;

use super::tls::RotatingCertificate;

/// How often the certificate files are re-read. Renewal is a rare event on a
/// long deadline, so noticing it within a minute is ample and costs two small
/// reads.
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// A handshake that never completes must not hold a connection slot open.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Serve the internal router until shutdown.
///
/// Without a certificate this is plain HTTP, which is the single-node and
/// development case. The channel carries fingerprints and counters rather than
/// user values, so an unencrypted hop inside a cluster is a deliberate and
/// stated position rather than an oversight.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    certificate: Option<Arc<RotatingCertificate>>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let Some(certificate) = certificate else {
        return crate::http::serve(listener, router, shutdown).await;
    };
    serve_tls(listener, router, certificate, REFRESH_INTERVAL, shutdown).await
}

/// The reload cadence is a parameter so a test can prove the serving path
/// picks up new material, rather than proving only that re-reading the files
/// works in isolation.
async fn serve_tls(
    listener: TcpListener,
    router: Router,
    certificate: Arc<RotatingCertificate>,
    refresh_interval: Duration,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let acceptor = tokio_rustls::TlsAcceptor::from(super::tls::server_config(certificate.clone()));
    let mut refresh = tokio::time::interval(refresh_interval);
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let service = hyper_util::service::TowerToHyperService::new(router);
    let mut connections = tokio::task::JoinSet::new();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            () = &mut shutdown => break,
            _ = refresh.tick() => {
                // A failed reload is not fatal: the previous certificate keeps
                // serving, and the next tick tries again.
                if let Err(error) = certificate.refresh() {
                    eprintln!("internal TLS certificate reload failed: {error}");
                }
            }
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    // One rejected connection must not stop the listener.
                    Err(_) => continue,
                };
                let acceptor = acceptor.clone();
                let service = service.clone();
                connections.spawn(async move {
                    let Ok(Ok(stream)) =
                        tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await
                    else {
                        return;
                    };
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                        .await;
                });
            }
        }
        // Reap finished connections so a long-lived listener does not
        // accumulate their handles.
        while connections.try_join_next().is_some() {}
    }
    connections.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Re-reading the files is one thing; the serving path actually doing it
    /// on its own is another, and only the second keeps a renewed certificate
    /// from being ignored until someone restarts the writer.
    #[tokio::test]
    async fn the_serving_path_reloads_renewed_material_by_itself() {
        let directory = tempfile::tempdir().unwrap();
        let files = super::super::TlsFiles {
            certificate: directory.path().join("tls.crt"),
            key: directory.path().join("tls.key"),
            authority: None,
        };
        let first = super::super::tests::self_signed("writer.test");
        std::fs::write(&files.certificate, &first.certificate).unwrap();
        std::fs::write(&files.key, &first.key).unwrap();
        let rotating = super::super::RotatingCertificate::load(files.clone()).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let served = tokio::spawn(serve_tls(
            listener,
            Router::new(),
            rotating.clone(),
            Duration::from_millis(20),
            async move {
                let _ = stopped.changed().await;
            },
        ));

        // Renewal rewrites the mounted files under the running server.
        let second = super::super::tests::self_signed("writer.test");
        std::fs::write(&files.certificate, &second.certificate).unwrap();
        std::fs::write(&files.key, &second.key).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let reloaded = loop {
            if rotating.serving_digest() != super::super::tls::pem_digest(&first.certificate) {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let _ = stop.send(true);
        let _ = served.await;
        assert!(
            reloaded,
            "the running server never picked up the renewed certificate"
        );
    }
}
