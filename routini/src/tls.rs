//! TLS certificate checks.
//!
//! TLS is provided by pingora's rustls backend. Its listener loads the certificate and key once,
//! when the listening service starts, and panics if they are unusable. Hence:
//! - [check_cert_and_key] loads them exactly like pingora does, so config validation (and
//!   `routini --check`) reports a bad certificate instead of a panic at startup
//! - certificates are not swapped at runtime, pingora's rustls listener has no certificate
//!   callback. Rotate them with a graceful upgrade (`routini --upgrade`, then SIGQUIT the old
//!   process). SIGHUP reports when the files changed on disk, see [fingerprint].

use std::hash::{DefaultHasher, Hash, Hasher};

use color_eyre::eyre::{Result, eyre};
use pingora::tls::{ServerConfig, load_certs_and_key_files, version};

/// Load the certificate chain and key and build a rustls server config from them, the same way
/// pingora's rustls listener will. Catches unreadable files, files without a certificate or
/// key, and a key that does not belong to the certificate.
pub fn check_cert_and_key(cert: &str, key: &str) -> Result<()> {
    let loaded = load_certs_and_key_files(cert, key)
        .map_err(|err| eyre!("failed to read certificate '{cert}' or key '{key}': {err}"))?;
    let Some((certs, private_key)) = loaded else {
        return Err(eyre!(
            "no certificate found in '{cert}' or no private key found in '{key}' \
             (expected PEM: CERTIFICATE / PRIVATE KEY, RSA PRIVATE KEY or EC PRIVATE KEY)"
        ));
    };
    ServerConfig::builder_with_protocol_versions(&[&version::TLS12, &version::TLS13])
        .with_no_client_auth()
        .with_single_cert(certs, private_key)
        .map_err(|err| eyre!("unusable certificate '{cert}' / key '{key}': {err}"))?;
    Ok(())
}

/// Hash of the certificate and key file contents, to tell on reload whether they changed on
/// disk. `None` if either can't be read.
pub fn fingerprint(cert: &str, key: &str) -> Option<u64> {
    let mut hasher = DefaultHasher::new();
    std::fs::read(cert).ok()?.hash(&mut hasher);
    std::fs::read(key).ok()?.hash(&mut hasher);
    Some(hasher.finish())
}

/// Self-signed certificates for tests.
#[cfg(test)]
pub(crate) mod test_certs {
    use std::path::{Path, PathBuf};

    /// Write a self-signed certificate for 127.0.0.1/localhost and its key into `dir`.
    pub fn write(dir: &Path) -> (String, String) {
        let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into(), "localhost".into()])
            .unwrap();
        let (cert_path, key_path) = (dir.join("cert.pem"), dir.join("key.pem"));
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();
        (
            cert_path.to_string_lossy().into_owned(),
            key_path.to_string_lossy().into_owned(),
        )
    }

    pub fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("routini-tls-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_certificate_is_accepted() {
        let (cert, key) = test_certs::write(&test_certs::temp_dir("valid"));
        check_cert_and_key(&cert, &key).unwrap();
    }

    #[test]
    fn missing_files_are_reported() {
        let err = check_cert_and_key("/nonexistent/c.pem", "/nonexistent/k.pem")
            .unwrap_err()
            .to_string();
        assert!(err.contains("/nonexistent/c.pem"), "{err}");
    }

    #[test]
    fn file_without_key_is_reported() {
        let (cert, _) = test_certs::write(&test_certs::temp_dir("nokey"));
        let err = check_cert_and_key(&cert, &cert).unwrap_err().to_string();
        assert!(err.contains("no private key"), "{err}");
    }

    #[test]
    fn key_of_another_certificate_is_rejected() {
        let (cert_a, _) = test_certs::write(&test_certs::temp_dir("a"));
        let (_, key_b) = test_certs::write(&test_certs::temp_dir("b"));
        let err = check_cert_and_key(&cert_a, &key_b).unwrap_err().to_string();
        assert!(err.contains("unusable certificate"), "{err}");
    }

    #[test]
    fn fingerprint_changes_with_file_contents() {
        let dir = test_certs::temp_dir("fingerprint");
        let (cert, key) = test_certs::write(&dir);
        let before = fingerprint(&cert, &key).unwrap();
        assert_eq!(fingerprint(&cert, &key), Some(before));
        test_certs::write(&dir);
        assert_ne!(fingerprint(&cert, &key), Some(before));
        assert_eq!(fingerprint("/nonexistent", "/nonexistent"), None);
    }
}
