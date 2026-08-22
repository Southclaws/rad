//! TLS for the instance-to-instance API.
//!
//! The certificate is issued outside this process — by cert-manager, or by
//! whoever supplied the Secret — and arrives as files in a mounted volume.
//! Renewal rewrites those files in place, so the material is re-read rather
//! than captured once: a certificate that renews every ninety days would
//! otherwise mean a writer restart every ninety days, or an expired
//! certificate served indefinitely.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// Where the certificate, its key, and the trust anchor live. Paths rather
/// than bytes: the files outlive any one read of them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TlsFiles {
    pub certificate: PathBuf,
    pub key: PathBuf,
    /// The authority a client verifies the certificate against. A writer does
    /// not need it to serve, but the same Secret carries it and a reader does.
    pub authority: Option<PathBuf>,
}

#[derive(Debug)]
pub enum TlsError {
    Unreadable(PathBuf, std::io::Error),
    NoCertificate(PathBuf),
    NoPrivateKey(PathBuf),
    Invalid(String),
}

impl std::fmt::Display for TlsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(path, error) => {
                write!(formatter, "cannot read {}: {error}", path.display())
            }
            Self::NoCertificate(path) => {
                write!(formatter, "{} holds no certificate", path.display())
            }
            Self::NoPrivateKey(path) => {
                write!(formatter, "{} holds no private key", path.display())
            }
            Self::Invalid(reason) => write!(formatter, "invalid TLS material: {reason}"),
        }
    }
}

impl std::error::Error for TlsError {}

/// rustls resolves its cryptography provider from a process-wide default, and
/// more than one is present in the dependency tree, so the choice has to be
/// stated. Installing is idempotent by way of ignoring the second attempt: two
/// instances of the same choice are not a conflict.
pub(super) fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn read(path: &Path) -> Result<Vec<u8>, TlsError> {
    std::fs::read(path).map_err(|error| TlsError::Unreadable(path.to_owned(), error))
}

pub(super) fn certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let bytes = read(path)?;
    let chain = rustls_pemfile::certs(&mut bytes.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| TlsError::Invalid(error.to_string()))?;
    if chain.is_empty() {
        return Err(TlsError::NoCertificate(path.to_owned()));
    }
    Ok(chain)
}

fn private_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let bytes = read(path)?;
    rustls_pemfile::private_key(&mut bytes.as_slice())
        .map_err(|error| TlsError::Invalid(error.to_string()))?
        .ok_or_else(|| TlsError::NoPrivateKey(path.to_owned()))
}

/// The certificate this instance serves, re-read whenever the files change.
///
/// rustls resolves the certificate per handshake, so a renewal takes effect on
/// the next connection with no restart and no dropped connection. Only a
/// successful load replaces the live keypair: a half-written file during
/// renewal leaves the previous certificate serving rather than breaking the
/// listener.
#[derive(Debug)]
pub struct RotatingCertificate {
    files: TlsFiles,
    current: std::sync::RwLock<Arc<rustls::sign::CertifiedKey>>,
    loaded: std::sync::Mutex<Fingerprint>,
}

/// What the files looked like when they were last loaded. Comparing content
/// rather than modification time avoids reloading on a touch and, more
/// usefully, still reloads when a rewrite keeps the timestamp.
type Fingerprint = (usize, u64);

fn fingerprint(certificate: &[u8], key: &[u8]) -> Fingerprint {
    let mut digest = crate::fnv::fnv1a(certificate);
    digest = crate::fnv::fnv1a_from(digest, key);
    (certificate.len() + key.len(), digest)
}

impl RotatingCertificate {
    pub fn load(files: TlsFiles) -> Result<Arc<Self>, TlsError> {
        install_crypto_provider();
        let (key, mark) = Self::read_keypair(&files)?;
        Ok(Arc::new(Self {
            files,
            current: std::sync::RwLock::new(key),
            loaded: std::sync::Mutex::new(mark),
        }))
    }

    fn read_keypair(
        files: &TlsFiles,
    ) -> Result<(Arc<rustls::sign::CertifiedKey>, Fingerprint), TlsError> {
        let certificate_bytes = read(&files.certificate)?;
        let key_bytes = read(&files.key)?;
        let chain = rustls_pemfile::certs(&mut certificate_bytes.as_slice())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| TlsError::Invalid(error.to_string()))?;
        if chain.is_empty() {
            return Err(TlsError::NoCertificate(files.certificate.clone()));
        }
        let key = private_key(&files.key)?;
        let signing = rustls::crypto::ring::sign::any_supported_type(&key)
            .map_err(|error| TlsError::Invalid(error.to_string()))?;
        let certified = rustls::sign::CertifiedKey::new(chain, signing);
        Ok((
            Arc::new(certified),
            fingerprint(&certificate_bytes, &key_bytes),
        ))
    }

    /// Re-read the files, replacing the live keypair only if they changed and
    /// the new material is usable. Returns whether the keypair was replaced.
    pub fn refresh(&self) -> Result<bool, TlsError> {
        let certificate_bytes = read(&self.files.certificate)?;
        let key_bytes = read(&self.files.key)?;
        let mark = fingerprint(&certificate_bytes, &key_bytes);
        {
            let loaded = self.loaded.lock().expect("tls fingerprint");
            if *loaded == mark {
                return Ok(false);
            }
        }
        let (key, mark) = Self::read_keypair(&self.files)?;
        *self.current.write().expect("tls keypair") = key;
        *self.loaded.lock().expect("tls fingerprint") = mark;
        Ok(true)
    }
}

impl RotatingCertificate {
    /// Digest of the certificate currently being served, so a test can tell
    /// which material a running server is presenting.
    #[cfg(test)]
    pub(super) fn serving_digest(&self) -> u64 {
        let current = self.current.read().expect("tls keypair");
        let mut digest = crate::fnv::OFFSET_BASIS;
        for certificate in current.cert.iter() {
            digest = crate::fnv::fnv1a_from(digest, certificate);
        }
        digest
    }
}

/// Digest of PEM-encoded certificates, comparable with [`RotatingCertificate::serving_digest`].
#[cfg(test)]
pub(super) fn pem_digest(pem: &[u8]) -> u64 {
    let chain = rustls_pemfile::certs(&mut &pem[..])
        .collect::<Result<Vec<_>, _>>()
        .expect("test certificate parses");
    let mut digest = crate::fnv::OFFSET_BASIS;
    for certificate in &chain {
        digest = crate::fnv::fnv1a_from(digest, certificate);
    }
    digest
}

impl rustls::server::ResolvesServerCert for RotatingCertificate {
    fn resolve(
        &self,
        _hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        Some(self.current.read().expect("tls keypair").clone())
    }
}

/// A server configuration that resolves its certificate per handshake.
///
/// Client certificates are not requested. TLS here answers "am I talking
/// privately to the actual writer?"; whether a caller may submit evidence is
/// the shared secret's question, and conflating the two would make rotating
/// either one a change to both.
pub(super) fn server_config(certificate: Arc<RotatingCertificate>) -> Arc<rustls::ServerConfig> {
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(certificate);
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renewal rewrites the mounted files in place. Serving the certificate
    /// read at boot would mean serving an expired one until someone restarts
    /// the writer.
    #[test]
    fn a_rewritten_certificate_is_picked_up_without_a_restart() {
        let directory = tempfile::tempdir().unwrap();
        let files = TlsFiles {
            certificate: directory.path().join("tls.crt"),
            key: directory.path().join("tls.key"),
            authority: None,
        };
        let first = super::super::tests::self_signed("writer.test");
        std::fs::write(&files.certificate, &first.certificate).unwrap();
        std::fs::write(&files.key, &first.key).unwrap();

        let rotating = RotatingCertificate::load(files.clone()).unwrap();
        let before = rotating.current.read().unwrap().clone();

        // Unchanged files must not churn the live keypair.
        assert!(!rotating.refresh().unwrap());
        assert!(Arc::ptr_eq(&before, &rotating.current.read().unwrap()));

        let second = super::super::tests::self_signed("writer.test");
        std::fs::write(&files.certificate, &second.certificate).unwrap();
        std::fs::write(&files.key, &second.key).unwrap();
        assert!(
            rotating.refresh().unwrap(),
            "a renewed certificate was ignored"
        );
        assert!(!Arc::ptr_eq(&before, &rotating.current.read().unwrap()));
    }

    /// Renewal is not atomic across two files. A half-written pair must leave
    /// the previous certificate serving rather than break the listener.
    #[test]
    fn unusable_material_leaves_the_previous_certificate_serving() {
        let directory = tempfile::tempdir().unwrap();
        let files = TlsFiles {
            certificate: directory.path().join("tls.crt"),
            key: directory.path().join("tls.key"),
            authority: None,
        };
        let good = super::super::tests::self_signed("writer.test");
        std::fs::write(&files.certificate, &good.certificate).unwrap();
        std::fs::write(&files.key, &good.key).unwrap();
        let rotating = RotatingCertificate::load(files.clone()).unwrap();
        let before = rotating.current.read().unwrap().clone();

        std::fs::write(
            &files.certificate,
            b"-----BEGIN CERTIFICATE-----\ntruncated\n",
        )
        .unwrap();
        assert!(rotating.refresh().is_err());
        assert!(
            Arc::ptr_eq(&before, &rotating.current.read().unwrap()),
            "a partial write replaced the serving certificate"
        );
    }

    #[test]
    fn material_that_was_never_usable_is_refused_at_load() {
        let directory = tempfile::tempdir().unwrap();
        let files = TlsFiles {
            certificate: directory.path().join("tls.crt"),
            key: directory.path().join("tls.key"),
            authority: None,
        };
        std::fs::write(&files.certificate, b"not a certificate").unwrap();
        std::fs::write(&files.key, b"not a key").unwrap();
        assert!(RotatingCertificate::load(files).is_err());
    }
}
