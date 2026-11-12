//! The far end of pairing: turn a code into a working identity.
//!
//! The key pair is generated HERE and the private key is written straight to
//! disk; only a certificate request crosses the network. Whoever runs the hub
//! never sees it, and neither does anyone watching the wire.

use anyhow::{Context, Result};

pub struct Paired {
    pub kind: String,
    pub name: String,
    pub identity_file: std::path::PathBuf,
}

/// Redeem `code` at `pair_url`, writing the result into `dir`.
///
/// `ca_pem` lets the caller pin the hub's certificate when it is self-signed
/// and already known; without it the hub must present a publicly trusted
/// certificate, because trusting whatever answers would defeat the point.
pub async fn pair(
    pair_url: &str,
    code: &str,
    dir: &str,
    hub_for_later: Option<&str>,
    ca_pem: Option<&str>,
    insecure: bool,
) -> Result<Paired> {
    // A placeholder subject: the hub replaces it with the name the code was
    // bound to, so there is nothing to be gained by lying here.
    let key = rcgen::KeyPair::generate().context("generating a key pair")?;
    let params = rcgen::CertificateParams::new(vec!["nexmesh-pairing".to_string()])
        .context("preparing the certificate request")?;
    let csr = params
        .serialize_request(&key)
        .context("creating the certificate request")?
        .pem()
        .context("encoding the certificate request")?;

    let mut b = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30));
    if let Some(pem) = ca_pem {
        let data = std::fs::read(pem).with_context(|| format!("reading {pem}"))?;
        b = b.add_root_certificate(
            reqwest::Certificate::from_pem(&data)
                .with_context(|| format!("parsing {pem} as a PEM certificate"))?,
        );
    }
    if insecure {
        // Only ever for a first contact the operator has decided to trust.
        b = b.danger_accept_invalid_certs(true);
    }
    let client = b.build()?;

    let url = format!("{}/v1/pair/claim", pair_url.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "code": code, "csr_pem": csr }))
        .send()
        .await
        .with_context(|| format!("reaching the pairing endpoint at {url}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        anyhow::bail!(
            "{}",
            v["error"].as_str().unwrap_or("pairing was refused")
        );
    }

    let kind = v["kind"].as_str().context("the hub returned no kind")?.to_string();
    let name = v["name"].as_str().context("the hub returned no name")?.to_string();
    let file = crate::identity::save(
        dir,
        &kind,
        &name,
        v["token"].as_str().context("the hub returned no token")?,
        v["ca_cert_pem"].as_str().context("the hub returned no CA certificate")?,
        v["cert_pem"].as_str().context("the hub returned no certificate")?,
        &key.serialize_pem(),
        hub_for_later,
    )
    .context("writing the identity")?;

    Ok(Paired { kind, name, identity_file: file })
}
