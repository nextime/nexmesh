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
/// bundle, and presenting a client certificate when the fleet requires one.
pub fn client_config(
    ca_pem: Option<&str>,
    client: Option<(&str, &str)>,
) -> Result<Arc<rustls::ClientConfig>> {
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

    let builder = rustls::ClientConfig::builder().with_root_certificates(roots);
    Ok(Arc::new(match client {
        None => builder.with_no_client_auth(),
        Some((cert_path, key_path)) => {
            let cert_data = std::fs::read(cert_path)
                .with_context(|| format!("reading client certificate {cert_path}"))?;
            let certs: Vec<_> = rustls_pemfile::certs(&mut &cert_data[..])
                .collect::<std::result::Result<_, _>>()
                .with_context(|| format!("parsing {cert_path}"))?;
            anyhow::ensure!(!certs.is_empty(), "{cert_path} contains no certificates");
            let key_data = std::fs::read(key_path)
                .with_context(|| format!("reading client key {key_path}"))?;
            let key = rustls_pemfile::private_key(&mut &key_data[..])
                .with_context(|| format!("parsing {key_path}"))?
                .with_context(|| format!("{key_path} contains no private key"))?;
            builder
                .with_client_auth_cert(certs, key)
                .context("the client certificate and key do not match")?
        }
    }))
}

/// Find a command on PATH, or in the descriptor's extra paths.
///
/// Lives here because the operator CLI has no other place for small path
/// helpers; it is the same logic the node uses to find an agent CLI.
pub fn noop_resolve(command: &str, extra: &[String]) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let expand = |p: &str| match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_string(),
    };
    let executable = |path: &str| {
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    if command.contains('/') {
        let p = expand(command);
        return executable(&p).then_some(p);
    }
    let path_var = std::env::var("PATH").unwrap_or_default();
    for dir in path_var.split(':').map(|s| s.to_string()).chain(extra.iter().map(|e| expand(e))) {
        if dir.is_empty() { continue; }
        let c = format!("{}/{}", dir.trim_end_matches('/'), command);
        if executable(&c) { return Some(c); }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_roots_alone_build_a_config() {
        assert!(client_config(None, None).is_ok());
    }

    #[test]
    fn a_missing_ca_file_is_a_clear_error() {
        let err = client_config(Some("/nonexistent/ca.pem"), None).unwrap_err();
        assert!(err.to_string().contains("reading CA file"), "{err}");
    }

    #[test]
    fn a_pem_with_no_certificates_is_rejected_rather_than_ignored() {
        let p = std::env::temp_dir().join(format!("nexmesh-noca-{}.pem", std::process::id()));
        std::fs::write(&p, "not a certificate\n").unwrap();
        let err = client_config(Some(p.to_str().unwrap()), None).unwrap_err();
        assert!(err.to_string().contains("no certificates"), "{err}");
        std::fs::remove_file(&p).ok();
    }
}
