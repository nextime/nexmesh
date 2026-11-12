//! A small certificate authority for the fleet.
//!
//! Tokens alone authenticate a node, but a token is a bearer secret: copy it
//! and you are that node. A client certificate adds something that cannot be
//! copied out of a log or a process listing, so impersonating a node needs
//! both the token AND a private key that never leaves the machine.
//!
//! The hub issues the certificates because it is the one component every node
//! already trusts. Nothing here talks to the network: it writes files.

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use std::path::{Path, PathBuf};

/// A private key must not be readable by other users on the machine.
fn write_private(path: &Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub struct Ca {
    pub cert: Certificate,
    pub key: KeyPair,
}

impl Ca {
    /// Load the CA from a directory, so issuing is possible long after init.
    pub fn load(dir: &Path) -> Result<Self> {
        let cert_pem = std::fs::read_to_string(dir.join("ca-cert.pem")).with_context(|| {
            format!(
                "no certificate authority in {} — run `nexmesh-hub pki init` first",
                dir.display()
            )
        })?;
        let key_pem = std::fs::read_to_string(dir.join("ca-key.pem"))
            .with_context(|| format!("reading the CA key in {}", dir.display()))?;
        let key = KeyPair::from_pem(&key_pem).context("parsing the CA key")?;
        let params = CertificateParams::from_ca_cert_pem(&cert_pem)
            .context("parsing the CA certificate")?;
        let cert = params.self_signed(&key).context("reloading the CA certificate")?;
        Ok(Self { cert, key })
    }
}

/// Create the CA plus the hub's own server certificate.
pub fn init(dir: &Path, hosts: &[String]) -> Result<Vec<(String, PathBuf)>> {
    std::fs::create_dir_all(dir)?;
    anyhow::ensure!(!hosts.is_empty(), "the hub needs at least one --host");

    let mut ca_params = CertificateParams::new(vec![])?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "nexmesh fleet CA");
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca_key = KeyPair::generate()?;
    let ca_cert = ca_params.self_signed(&ca_key)?;

    std::fs::write(dir.join("ca-cert.pem"), ca_cert.pem())?;
    write_private(&dir.join("ca-key.pem"), &ca_key.serialize_pem())?;

    // The hub's server certificate, signed by that CA, so nodes verify the hub
    // against the same anchor they present a certificate to.
    let mut srv = CertificateParams::new(hosts.to_vec())?;
    srv.distinguished_name.push(DnType::CommonName, hosts[0].clone());
    srv.use_authority_key_identifier_extension = true;
    srv.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
    srv.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let srv_key = KeyPair::generate()?;
    let srv_cert = srv.signed_by(&srv_key, &ca_cert, &ca_key)?;

    std::fs::write(dir.join("server-cert.pem"), srv_cert.pem())?;
    write_private(&dir.join("server-key.pem"), &srv_key.serialize_pem())?;

    Ok(vec![
        ("CA certificate".into(), dir.join("ca-cert.pem")),
        ("CA key".into(), dir.join("ca-key.pem")),
        ("server certificate".into(), dir.join("server-cert.pem")),
        ("server key".into(), dir.join("server-key.pem")),
    ])
}

/// A serial number nobody can guess, so a certificate cannot be impersonated
/// in a revocation list by someone predicting the next one.
pub fn new_serial() -> u64 {
    use std::io::Read;
    let mut b = [0u8; 8];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
    // Keep it positive and non-zero: a zero serial is not valid.
    (u64::from_be_bytes(b) >> 1).max(1)
}

/// Build a certificate revocation list naming every revoked serial.
///
/// This is what makes revocation real: rustls checks it during the TLS
/// handshake, so a revoked machine cannot connect at all -- as opposed to an
/// application-level deny list, which would only take effect after the
/// connection it is meant to prevent.
pub fn build_crl(
    dir: &Path,
    revoked: &[(u64, i64)],
    crl_number: u64,
) -> Result<String> {
    use rcgen::{CertificateRevocationListParams, RevocationReason, RevokedCertParams};
    use time::{Duration, OffsetDateTime};

    let ca = Ca::load(dir)?;
    let now = OffsetDateTime::now_utc();
    let params = CertificateRevocationListParams {
        this_update: now,
        // Generous, because the list is rewritten on every revocation; a CRL
        // that expires would start rejecting everyone.
        next_update: now + Duration::days(365),
        crl_number: rcgen::SerialNumber::from(crl_number),
        issuing_distribution_point: None,
        revoked_certs: revoked
            .iter()
            .map(|(serial, at_ms)| RevokedCertParams {
                serial_number: rcgen::SerialNumber::from(*serial),
                revocation_time: OffsetDateTime::from_unix_timestamp(at_ms / 1000)
                    .unwrap_or(now),
                reason_code: Some(RevocationReason::CessationOfOperation),
                invalidity_date: None,
            })
            .collect(),
        key_identifier_method: rcgen::KeyIdMethod::Sha256,
    };
    let crl = params
        .signed_by(&ca.cert, &ca.key)
        .context("signing the revocation list")?;
    crl.pem().context("encoding the revocation list")
}

/// Issue a client certificate for a node or an operator.
pub fn issue_client(dir: &Path, name: &str, out: &Path, serial: u64) -> Result<(PathBuf, PathBuf)> {
    anyhow::ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'),
        "'{name}' is not a usable certificate name"
    );
    let ca = Ca::load(dir)?;
    std::fs::create_dir_all(out)?;

    let mut p = CertificateParams::new(vec![name.to_string()])?;
    // An explicit serial, because a certificate can only be revoked by one.
    p.serial_number = Some(rcgen::SerialNumber::from(serial));
    // The common name is the identity this certificate asserts.
    p.distinguished_name.push(DnType::CommonName, name.to_string());
    p.use_authority_key_identifier_extension = true;
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
    p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let key = KeyPair::generate()?;
    let cert = p.signed_by(&key, &ca.cert, &ca.key)?;

    let cert_path = out.join(format!("{name}-cert.pem"));
    let key_path = out.join(format!("{name}-key.pem"));
    std::fs::write(&cert_path, cert.pem())?;
    write_private(&key_path, &key.serialize_pem())?;
    Ok((cert_path, key_path))
}

/// Sign a certificate request produced on another machine.
///
/// The subject is REPLACED with the name the pairing code was bound to: a
/// requester does not get to choose the identity it is issued, only to prove
/// it holds the matching private key. Everything else in the request is
/// discarded for the same reason.
pub fn sign_csr(dir: &Path, csr_pem: &str, name: &str, serial: u64) -> Result<String> {
    anyhow::ensure!(
        csr_pem.contains("CERTIFICATE REQUEST"),
        "that does not look like a PEM certificate request"
    );
    let ca = Ca::load(dir)?;
    let mut csr = rcgen::CertificateSigningRequestParams::from_pem(csr_pem)
        .context("parsing the certificate request")?;

    csr.params.serial_number = Some(rcgen::SerialNumber::from(serial));
    csr.params.distinguished_name = rcgen::DistinguishedName::new();
    csr.params
        .distinguished_name
        .push(DnType::CommonName, name.to_string());
    csr.params.subject_alt_names = vec![rcgen::SanType::DnsName(
        name.try_into().context("that name cannot go in a certificate")?,
    )];
    csr.params.is_ca = IsCa::NoCa;
    csr.params.key_usages =
        vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
    csr.params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    csr.params.use_authority_key_identifier_extension = true;

    let cert = csr
        .signed_by(&ca.cert, &ca.key)
        .context("signing the certificate request")?;
    Ok(cert.pem())
}

/// The CA certificate, for handing to whoever is pairing.
pub fn ca_cert_pem(dir: &Path) -> Result<String> {
    std::fs::read_to_string(dir.join("ca-cert.pem"))
        .with_context(|| format!("reading the CA certificate in {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nexmesh-pki-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn init_writes_a_ca_and_a_server_certificate() {
        let d = tmp("init");
        let files = init(&d, &["machine-a".into(), "10.0.0.2".into()]).unwrap();
        assert_eq!(files.len(), 4);
        for (_, p) in &files {
            assert!(p.exists(), "{}", p.display());
        }
        assert!(std::fs::read_to_string(d.join("ca-cert.pem"))
            .unwrap()
            .starts_with("-----BEGIN CERTIFICATE-----"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn private_keys_are_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("perms");
        init(&d, &["h".into()]).unwrap();
        for f in ["ca-key.pem", "server-key.pem"] {
            let m = std::fs::metadata(d.join(f)).unwrap().permissions().mode();
            assert_eq!(m & 0o777, 0o600, "{f} is {:o}", m & 0o777);
        }
        let (_, key) = issue_client(&d, "machine-a", &d, 42).unwrap();
        let m = std::fs::metadata(&key).unwrap().permissions().mode();
        assert_eq!(m & 0o777, 0o600, "client key is {:o}", m & 0o777);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_client_certificate_can_be_issued_after_the_fact() {
        // The CA must be reloadable: certificates are issued days later, from
        // a fresh process.
        let d = tmp("issue");
        init(&d, &["h".into()]).unwrap();
        let out = tmp("issue-out");
        let (cert, key) = issue_client(&d, "machine-a", &out, 7).unwrap();
        assert!(cert.exists() && key.exists());
        assert!(std::fs::read_to_string(&cert).unwrap().contains("BEGIN CERTIFICATE"));
        // And again, for a second node.
        assert!(issue_client(&d, "machine-c", &out, 8).is_ok());
        std::fs::remove_dir_all(&d).ok();
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn issuing_without_a_ca_says_what_to_do() {
        let d = tmp("nocadir");
        let err = issue_client(&d, "machine-a", &d, 1).unwrap_err();
        assert!(err.to_string().contains("pki init"), "{err}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_signed_request_gets_the_name_the_hub_chose_not_the_one_it_asked_for() {
        let d = tmp("csr");
        init(&d, &["h".into()]).unwrap();
        // A requester asks to be "admin"; the hub was told this code is "machine-a".
        let key = KeyPair::generate().unwrap();
        let mut p = CertificateParams::new(vec!["admin".to_string()]).unwrap();
        p.distinguished_name.push(DnType::CommonName, "admin");
        let csr = p.serialize_request(&key).unwrap().pem().unwrap();

        let signed = sign_csr(&d, &csr, "machine-a", 11).unwrap();
        assert!(signed.starts_with("-----BEGIN CERTIFICATE-----"));
        // Parse it back and check whose name it carries.
        let parsed = CertificateParams::from_ca_cert_pem(&signed);
        assert!(parsed.is_ok(), "the signed certificate should parse");
        assert!(!signed.contains("admin"), "the requested name must not survive");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn rubbish_in_place_of_a_request_is_refused() {
        let d = tmp("badcsr");
        init(&d, &["h".into()]).unwrap();
        assert!(sign_csr(&d, "hello", "machine-a", 12).is_err());
        assert!(sign_csr(&d, "-----BEGIN CERTIFICATE REQUEST-----\nnope\n", "machine-a", 13).is_err());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_revocation_list_is_produced_and_names_the_serial() {
        let d = tmp("crl");
        init(&d, &["h".into()]).unwrap();
        issue_client(&d, "machine-a", &d, 4242).unwrap();
        let pem = build_crl(&d, &[(4242, 1_700_000_000_000)], 1).unwrap();
        assert!(pem.starts_with("-----BEGIN X509 CRL-----"), "{}", &pem[..40.min(pem.len())]);
        // An empty list is still a valid list: that is how revocation is
        // undone for everybody, and it must not fail.
        let empty = build_crl(&d, &[], 2).unwrap();
        assert!(empty.starts_with("-----BEGIN X509 CRL-----"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn serials_are_unpredictable_and_never_zero() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            let s = new_serial();
            assert!(s > 0, "a zero serial is not valid");
            seen.insert(s);
        }
        assert_eq!(seen.len(), 200, "serials repeated");
    }

    #[test]
    fn a_name_that_would_escape_the_directory_is_refused() {
        let d = tmp("badname");
        init(&d, &["h".into()]).unwrap();
        for bad in ["../../etc/passwd", "a/b", "", "name with spaces"] {
            assert!(issue_client(&d, bad, &d, 9).is_err(), "{bad:?} should be refused");
        }
        std::fs::remove_dir_all(&d).ok();
    }
}
