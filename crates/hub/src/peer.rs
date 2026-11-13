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

use axum::http::HeaderMap;
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

/// Read the identity a trusted reverse proxy verified.
///
/// Behind a proxy the hub sees the proxy's connection, not the client's, so
/// the certificate it would otherwise inspect is not there. nginx can verify
/// the client certificate itself and forward the result, which keeps the
/// binding working -- but a header is only evidence if nothing else can set
/// it, so this is used only for requests arriving from an address the
/// operator nominated with `--trusted-proxy`.
///
/// `X-Client-Verify` is checked before `X-Client-CN` deliberately: nginx
/// forwards a common name even for a certificate it rejected, so taking the
/// name alone would accept an untrusted certificate's claim.
pub fn from_proxy(headers: &HeaderMap) -> Option<PeerIdentity> {
    let verify = headers.get("x-client-verify").and_then(|v| v.to_str().ok())?;
    if verify.eq_ignore_ascii_case("success") {
        // The bare common name where the proxy could extract it, else the
        // whole subject DN. nginx has no variable for the CN alone, so the
        // extraction is a regex in its config -- and a regex against a
        // certificate subject is exactly the thing that quietly stops
        // matching, so the DN is accepted as a fallback.
        let cn = headers
            .get("x-client-cn")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                headers
                    .get("x-client-dn")
                    .and_then(|v| v.to_str().ok())
                    .and_then(common_name_from_dn)
            });
        return Some(PeerIdentity { presented: true, common_name: cn });
    }
    // NONE means the client offered nothing; FAILED means it offered
    // something invalid. Neither is an identity, and FAILED should never have
    // reached us -- nginx was asked to verify.
    Some(PeerIdentity { presented: false, common_name: None })
}

/// Pull the common name out of a subject DN.
///
/// Both orderings appear in the wild -- `CN=machine-b,O=nexmesh` from one
/// toolchain and `/O=nexmesh/CN=machine-b` from another -- so the separator is
/// either a comma or a slash.
pub fn common_name_from_dn(dn: &str) -> Option<String> {
    dn.split([',', '/'])
        .map(str::trim)
        .find_map(|part| {
            let (k, v) = part.split_once('=')?;
            k.trim().eq_ignore_ascii_case("cn").then(|| v.trim().to_string())
        })
        .filter(|s| !s.is_empty())
}

/// Is this request coming from a proxy the operator nominated?
pub fn is_trusted_proxy(trusted: &[std::net::IpAddr], peer: Option<std::net::IpAddr>) -> bool {
    match peer {
        Some(ip) => trusted.iter().any(|t| *t == ip),
        // No socket address to check means this cannot be established, and
        // guessing in the permissive direction would make the flag worthless.
        None => false,
    }
}

/// Who this request is from: its own certificate, or the one a nominated
/// proxy verified.
///
/// The connection's own certificate wins when there is one. A proxy's headers
/// are consulted only when the request arrives from an address the operator
/// nominated, because those headers are trivially forgeable by anyone who can
/// reach the hub directly -- which is why nominating a proxy also means
/// binding the hub where only that proxy can reach it.
pub fn resolve<B>(st: &crate::state::AppState, req: &axum::http::Request<B>)
    -> std::sync::Arc<PeerIdentity>
{
    let own = req.extensions().get::<std::sync::Arc<PeerIdentity>>().cloned();
    let from = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip());
    resolve_parts(st, own, from, req.headers())
}

/// The same, from pieces an extractor can supply.
///
/// A WebSocket handler cannot take the whole request and the upgrade at once,
/// so it takes these instead; both paths must resolve identity identically or
/// a node arriving through a proxy would be bound differently from every
/// other route.
pub fn resolve_parts(
    st: &crate::state::AppState,
    own: Option<std::sync::Arc<PeerIdentity>>,
    from: Option<std::net::IpAddr>,
    headers: &HeaderMap,
) -> std::sync::Arc<PeerIdentity> {
    // A nominated proxy's verdict comes FIRST, before the connection's own
    // certificate. Where nginx terminates TLS and talks to the hub over its
    // own mutual-TLS connection, that connection legitimately presents
    // nginx's certificate -- and binding a client's token to the proxy's name
    // would reject every request. The proxy is trusted precisely to say who
    // the client is, so that is what is believed.
    let trusted = st.trusted_proxies();
    if !trusted.is_empty() && is_trusted_proxy(trusted, from) {
        if let Some(p) = from_proxy(headers) {
            return std::sync::Arc::new(p);
        }
    }
    own.unwrap_or_default()
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

    fn hdr(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn a_proxy_verified_certificate_is_an_identity() {
        let p = from_proxy(&hdr(&[("x-client-verify", "SUCCESS"), ("x-client-cn", "machine-b")]))
            .unwrap();
        assert!(p.presented);
        assert_eq!(p.common_name.as_deref(), Some("machine-b"));
        assert!(p.agrees_with("machine-b").is_ok());
        assert!(p.agrees_with("someone-else").is_err());
    }

    #[test]
    fn a_subject_dn_is_accepted_when_the_proxy_could_not_extract_the_name() {
        // nginx has no variable for the CN alone, so its config extracts it
        // with a regex. A regex against a certificate subject is exactly what
        // quietly stops matching, so the whole DN is a fallback.
        for dn in [
            "CN=machine-b",
            "CN=machine-b,O=nexmesh",
            "/O=nexmesh/CN=machine-b",
            "emailAddress=a@b,CN=machine-b,O=x",
        ] {
            let p = from_proxy(&hdr(&[("x-client-verify", "SUCCESS"), ("x-client-dn", dn)]))
                .unwrap();
            assert_eq!(p.common_name.as_deref(), Some("machine-b"), "{dn}");
        }
    }

    #[test]
    fn the_extracted_name_is_preferred_over_the_dn() {
        let p = from_proxy(&hdr(&[
            ("x-client-verify", "SUCCESS"),
            ("x-client-cn", "machine-b"),
            ("x-client-dn", "CN=someone-else"),
        ]))
        .unwrap();
        assert_eq!(p.common_name.as_deref(), Some("machine-b"));
    }

    #[test]
    fn a_dn_with_no_common_name_yields_nothing_rather_than_junk() {
        assert!(common_name_from_dn("O=nexmesh,L=somewhere").is_none());
        assert!(common_name_from_dn("").is_none());
        assert!(common_name_from_dn("CN=").is_none());
    }

    #[test]
    fn a_certificate_the_proxy_rejected_is_not_an_identity() {
        // nginx forwards the common name even for a certificate it refused,
        // so reading the name without the verdict would accept it.
        let p = from_proxy(&hdr(&[
            ("x-client-verify", "FAILED:self signed certificate"),
            ("x-client-cn", "intruder"),
        ]))
        .unwrap();
        assert!(!p.presented, "a rejected certificate asserts nothing");
        assert_eq!(p.common_name, None);
    }

    #[test]
    fn no_certificate_through_a_proxy_is_simply_no_certificate() {
        let p = from_proxy(&hdr(&[("x-client-verify", "NONE")])).unwrap();
        assert!(!p.presented);
        // So the token stands alone, as on a hub without mutual TLS.
        assert!(p.agrees_with("anyone").is_ok());
    }

    #[test]
    fn without_the_verify_header_there_is_no_proxy_identity_at_all() {
        // A bare X-Client-CN must never be believed: that is precisely the
        // header an attacker would set.
        assert!(from_proxy(&hdr(&[("x-client-cn", "machine-b")])).is_none());
        assert!(from_proxy(&HeaderMap::new()).is_none());
    }

    #[test]
    fn a_nominated_proxy_speaks_for_the_client_not_for_itself() {
        // The case that made this necessary: nginx terminates public TLS and
        // reaches the hub over its own mutual-TLS connection. That connection
        // really does present nginx's certificate, so preferring the
        // connection's own identity would bind every client's token to the
        // name "nginx" and refuse the lot.
        let st = crate::state::AppState::in_memory()
            .unwrap()
            .with_trusted_proxies(vec!["127.0.0.1".parse().unwrap()]);
        let nginx_own = std::sync::Arc::new(PeerIdentity {
            presented: true,
            common_name: Some("nginx-proxy".into()),
        });
        let got = resolve_parts(
            &st,
            Some(nginx_own.clone()),
            Some("127.0.0.1".parse().unwrap()),
            &hdr(&[("x-client-verify", "SUCCESS"), ("x-client-cn", "machine-b")]),
        );
        assert_eq!(got.common_name.as_deref(), Some("machine-b"));

        // From anywhere else, the headers are ignored and the connection's
        // own certificate is what counts.
        let got = resolve_parts(
            &st,
            Some(nginx_own),
            Some("10.0.0.9".parse().unwrap()),
            &hdr(&[("x-client-verify", "SUCCESS"), ("x-client-cn", "machine-b")]),
        );
        assert_eq!(
            got.common_name.as_deref(),
            Some("nginx-proxy"),
            "a header from an un-nominated address must not be believed"
        );
    }

    #[test]
    fn with_no_proxy_nominated_only_the_connection_counts() {
        let st = crate::state::AppState::in_memory().unwrap();
        let own = std::sync::Arc::new(PeerIdentity {
            presented: true,
            common_name: Some("machine-a".into()),
        });
        let got = resolve_parts(
            &st,
            Some(own),
            Some("127.0.0.1".parse().unwrap()),
            &hdr(&[("x-client-verify", "SUCCESS"), ("x-client-cn", "impostor")]),
        );
        assert_eq!(got.common_name.as_deref(), Some("machine-a"));
    }

    #[test]
    fn only_a_nominated_proxy_address_is_trusted() {
        let trusted: Vec<std::net::IpAddr> = vec!["127.0.0.1".parse().unwrap()];
        assert!(is_trusted_proxy(&trusted, Some("127.0.0.1".parse().unwrap())));
        assert!(!is_trusted_proxy(&trusted, Some("10.0.0.9".parse().unwrap())));
        assert!(!is_trusted_proxy(&trusted, None));
        assert!(
            !is_trusted_proxy(&[], Some("127.0.0.1".parse().unwrap())),
            "with no proxy nominated, nothing is trusted"
        );
    }

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
