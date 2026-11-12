//! TLS trust for talking to the hub.
//!
//! A fleet hub usually has a self-signed certificate (`nexmesh-hub gen-cert`),
//! which no public CA vouches for. Rather than offer a "skip verification"
//! switch -- which would defeat the point, since an attacker who can redirect
//! the connection is exactly who the token must be protected from -- a node is
//! given the hub's certificate as an explicit trust anchor.

use anyhow::{Context, Result};
use std::sync::Arc;

/// Build a TLS config trusting the public CAs plus, if given, one extra PEM
/// bundle. Public roots stay in place so a hub behind a real certificate works
/// with no extra configuration.
pub fn client_config(ca_pem: Option<&str>) -> Result<Arc<rustls::ClientConfig>> {
    // rustls 0.23 needs a crypto provider chosen for the process.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    if let Some(path) = ca_pem {
        let data = std::fs::read(path).with_context(|| format!("reading CA file {path}"))?;
        let mut reader = std::io::BufReader::new(&data[..]);
        let mut added = 0usize;
        for cert in rustls_pemfile::certs(&mut reader) {
            let cert = cert.with_context(|| format!("parsing a certificate in {path}"))?;
            roots
                .add(cert)
                .with_context(|| format!("adding a certificate from {path}"))?;
            added += 1;
        }
        // Silently trusting nothing extra would leave a confusing TLS failure.
        anyhow::ensure!(added > 0, "{path} contains no certificates");
        let _ = (path, added); // the operator CLI has no tracing subscriber
    }

    Ok(Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_roots_alone_build_a_config() {
        assert!(client_config(None).is_ok());
    }

    #[test]
    fn a_missing_ca_file_is_a_clear_error() {
        let err = client_config(Some("/nonexistent/ca.pem")).unwrap_err();
        assert!(err.to_string().contains("reading CA file"), "{err}");
    }

    #[test]
    fn a_pem_with_no_certificates_is_rejected_rather_than_ignored() {
        let p = std::env::temp_dir().join(format!("nexmesh-noca-{}.pem", std::process::id()));
        std::fs::write(&p, "not a certificate\n").unwrap();
        let err = client_config(Some(p.to_str().unwrap())).unwrap_err();
        assert!(err.to_string().contains("no certificates"), "{err}");
        std::fs::remove_file(&p).ok();
    }
}
