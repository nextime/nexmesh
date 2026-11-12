//! Who is at the other end of the TLS connection.
//!
//! Mutual TLS proves the caller holds a certificate this CA issued. It does
//! not, on its own, say the caller is *who its token claims to be*: any
//! certificate the CA signed could be presented alongside any token the hub
//! issued. Binding the two closes that gap, so a stolen node token is useless
//! without that node's own key, and an operator's certificate cannot be used
//! to impersonate a machine.
//!
//! The peer's certificate is not visible to a request handler by default, so
//! the TLS acceptor is wrapped: after the handshake it reads the common name
//! off the presented certificate and attaches it to every request on that
//! connection.

use axum_server::accept::Accept;
use axum_server::tls_rustls::RustlsAcceptor;
use std::{future::Future, io, pin::Pin, sync::Arc, task::{Context, Poll}};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::server::TlsStream;

/// What the TLS layer established about the caller.
#[derive(Debug, Clone, Default)]
pub struct PeerIdentity {
    /// Whether a client certificate was presented at all. False means this
    /// hub is not asking for one, and there is nothing to bind to.
    pub presented: bool,
    /// The certificate's common name: the identity it asserts.
    pub common_name: Option<String>,
}

impl PeerIdentity {
    /// Does this connection's certificate agree that the caller is `name`?
    ///
    /// With no certificate there is nothing to check, and the token stands
    /// alone -- which is the situation on a hub running without mutual TLS.
    pub fn agrees_with(&self, name: &str) -> Result<(), String> {
        if !self.presented {
            return Ok(());
        }
        match self.common_name.as_deref() {
            Some(cn) if cn == name => Ok(()),
            Some(cn) => Err(format!(
                "this connection's certificate was issued to '{cn}', but the credential claims \
                 to be '{name}'. A certificate and a token must belong to the same identity."
            )),
            None => Err(
                "this connection's certificate carries no common name, so it cannot be matched \
                 to a credential. Re-issue it with `nexmesh-hub pki issue`."
                    .into(),
            ),
        }
    }
}

/// Read the common name out of a DER certificate.
pub fn common_name(der: &[u8]) -> Option<String> {
    use x509_parser::prelude::*;
    let (_, cert) = X509Certificate::from_der(der).ok()?;
    let subject = cert.subject();
    let attr = subject.iter_common_name().next()?;
    let name = attr.as_str().ok()?.to_string();
    Some(name)
}

/// A TLS acceptor that records who connected.
#[derive(Clone)]
pub struct IdentityAcceptor {
    inner: RustlsAcceptor,
}

impl IdentityAcceptor {
    pub fn new(inner: RustlsAcceptor) -> Self {
        Self { inner }
    }
}

impl<I, S> Accept<I, S> for IdentityAcceptor
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    S: Send + 'static,
{
    type Stream = TlsStream<I>;
    type Service = WithPeer<S>;
    type Future = Pin<Box<dyn Future<Output = io::Result<(Self::Stream, Self::Service)>> + Send>>;

    fn accept(&self, stream: I, service: S) -> Self::Future {
        let inner = self.inner.clone();
        Box::pin(async move {
            let (tls, service) = inner.accept(stream, service).await?;
            // The handshake is done, so the peer's certificate chain is known.
            let (_, conn) = tls.get_ref();
            let certs = conn.peer_certificates();
            let peer = PeerIdentity {
                presented: certs.map(|c| !c.is_empty()).unwrap_or(false),
                common_name: certs.and_then(|c| c.first()).and_then(|c| common_name(c)),
            };
            Ok((tls, WithPeer { inner: service, peer: Arc::new(peer) }))
        })
    }
}

/// Attaches the peer identity to every request on one connection.
#[derive(Clone)]
pub struct WithPeer<S> {
    inner: S,
    peer: Arc<PeerIdentity>,
}

impl<S, B> tower_service::Service<axum::http::Request<B>> for WithPeer<S>
where
    S: tower_service::Service<axum::http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: axum::http::Request<B>) -> Self::Future {
        req.extensions_mut().insert(self.peer.clone());
        self.inner.call(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_certificate_means_nothing_to_bind() {
        // A hub without mutual TLS still works; the token stands alone.
        let p = PeerIdentity::default();
        assert!(p.agrees_with("machine-a").is_ok());
        assert!(p.agrees_with("anyone-at-all").is_ok());
    }

    #[test]
    fn a_matching_certificate_is_accepted() {
        let p = PeerIdentity { presented: true, common_name: Some("machine-a".into()) };
        assert!(p.agrees_with("machine-a").is_ok());
    }

    #[test]
    fn a_certificate_for_someone_else_is_refused() {
        // The whole point: an operator's certificate must not be usable with a
        // machine's token, nor one machine's with another's.
        let p = PeerIdentity { presented: true, common_name: Some("operator".into()) };
        let err = p.agrees_with("machine-a").unwrap_err();
        assert!(err.contains("operator") && err.contains("machine-a"), "{err}");
    }

    #[test]
    fn a_certificate_without_a_name_cannot_be_bound() {
        let p = PeerIdentity { presented: true, common_name: None };
        let err = p.agrees_with("machine-a").unwrap_err();
        assert!(err.contains("no common name"), "{err}");
    }

    #[test]
    fn the_comparison_is_exact() {
        // Not a prefix, not case-insensitive: these are issued names.
        let p = PeerIdentity { presented: true, common_name: Some("machine-a".into()) };
        for wrong in ["machine-a2", "machine", "MACHINE-A", "machine-a "] {
            assert!(p.agrees_with(wrong).is_err(), "{wrong:?} should not match");
        }
    }
}
