//! A paired identity on disk.
//!
//! Pairing writes everything a node or operator needs into one directory, and
//! the binaries read it from there. The point is that nobody should have to
//! copy certificates around or remember four flags: pair once, then run.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_DIR: &str = "~/.nexmesh";
pub const FILE: &str = "identity.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    /// "node" or "operator".
    pub kind: String,
    /// The name this identity was issued for.
    pub name: String,
    /// Bearer token: a node enrolment token, or an operator token.
    pub token: String,
    /// File names, relative to the directory holding this file.
    pub ca_cert: String,
    pub cert: String,
    pub key: String,
    /// Where it was paired, so `nexmesh` needs no --hub afterwards.
    #[serde(default)]
    pub hub: Option<String>,
}

pub fn expand(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_string(),
    }
}

#[derive(Debug, Clone)]
pub struct Loaded {
    pub identity: Identity,
    pub dir: PathBuf,
}

impl Loaded {
    pub fn ca_cert_path(&self) -> String {
        self.dir.join(&self.identity.ca_cert).to_string_lossy().to_string()
    }
    pub fn cert_path(&self) -> String {
        self.dir.join(&self.identity.cert).to_string_lossy().to_string()
    }
    pub fn key_path(&self) -> String {
        self.dir.join(&self.identity.key).to_string_lossy().to_string()
    }
    /// Only usable if every file it names is actually there.
    pub fn complete(&self) -> bool {
        [self.ca_cert_path(), self.cert_path(), self.key_path()]
            .iter()
            .all(|p| Path::new(p).exists())
    }
}

/// Read a paired identity, if one is there. A missing directory is not an
/// error: plenty of setups run without TLS at all.
pub fn load(dir: &str) -> Option<Loaded> {
    let dir = PathBuf::from(expand(dir));
    let text = std::fs::read_to_string(dir.join(FILE)).ok()?;
    let identity: Identity = serde_json::from_str(&text).ok()?;
    let loaded = Loaded { identity, dir };
    loaded.complete().then_some(loaded)
}

/// Write one out, keeping the private key to this user.
pub fn save(
    dir: &str,
    kind: &str,
    name: &str,
    token: &str,
    ca_pem: &str,
    cert_pem: &str,
    key_pem: &str,
    hub: Option<&str>,
) -> std::io::Result<PathBuf> {
    let dir = PathBuf::from(expand(dir));
    std::fs::create_dir_all(&dir)?;
    let ca = "ca-cert.pem".to_string();
    let cert = format!("{name}-cert.pem");
    let key = format!("{name}-key.pem");
    std::fs::write(dir.join(&ca), ca_pem)?;
    std::fs::write(dir.join(&cert), cert_pem)?;
    std::fs::write(dir.join(&key), key_pem)?;
    let identity = Identity {
        kind: kind.into(),
        name: name.into(),
        token: token.into(),
        ca_cert: ca,
        cert,
        key: key.clone(),
        hub: hub.map(String::from),
    };
    let file = dir.join(FILE);
    std::fs::write(&file, serde_json::to_string_pretty(&identity)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The key and the token are both secrets.
        std::fs::set_permissions(dir.join(&key), std::fs::Permissions::from_mode(0o600))?;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> String {
        let d = std::env::temp_dir().join(format!("nexmesh-id-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d.to_string_lossy().to_string()
    }

    #[test]
    fn a_saved_identity_round_trips() {
        let d = tmp("rt");
        save(&d, "node", "machine-a", "tok", "CA", "CERT", "KEY", Some("wss://h:7870")).unwrap();
        let l = load(&d).expect("should load");
        assert_eq!(l.identity.name, "machine-a");
        assert_eq!(l.identity.token, "tok");
        assert_eq!(l.identity.hub.as_deref(), Some("wss://h:7870"));
        assert!(l.complete());
        assert_eq!(std::fs::read_to_string(l.cert_path()).unwrap(), "CERT");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn secrets_are_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("perm");
        save(&d, "node", "machine-a", "tok", "CA", "CERT", "KEY", None).unwrap();
        for f in ["machine-a-key.pem", FILE] {
            let m = std::fs::metadata(format!("{d}/{f}")).unwrap().permissions().mode();
            assert_eq!(m & 0o777, 0o600, "{f} is {:o}", m & 0o777);
        }
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn no_identity_is_not_an_error() {
        assert!(load(&tmp("absent")).is_none());
    }

    #[test]
    fn an_identity_missing_its_files_is_ignored_rather_than_half_used() {
        // Half a credential set would fail later, somewhere less obvious.
        let d = tmp("partial");
        save(&d, "node", "machine-a", "tok", "CA", "CERT", "KEY", None).unwrap();
        std::fs::remove_file(format!("{d}/machine-a-key.pem")).unwrap();
        assert!(load(&d).is_none());
        std::fs::remove_dir_all(&d).ok();
    }
}
