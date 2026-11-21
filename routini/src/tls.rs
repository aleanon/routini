//! TLS termination: certificates selected by SNI, reloadable, and optional client certificate
//! authentication.
//!
//! Pingora's rustls listener takes the [ServerConfig] built here
//! (`TlsSettings::with_server_config`). Its certificates come from a [CertResolver], a
//! lock-free swappable [CertStore]:
//! - the default certificate is served without SNI, or when no other certificate's names match
//! - every certificate also serves the DNS names it lists (SAN, else CN), wildcards included,
//!   like one nginx `server` block per certificate
//! - a config reload loads the files again and swaps the store, so renewed certificates are
//!   served by new connections without a restart; connections already open keep theirs
//!
//! Protocol versions and client authentication are part of the [ServerConfig] itself and can
//! only change with a restart or graceful upgrade.

use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    sync::Arc,
};

use arc_swap::ArcSwap;
use color_eyre::eyre::{Result, WrapErr, eyre};
use pingora::tls::{
    CertificateDer, RootCertStore, ServerConfig, load_certs_and_key_files, version,
};
use rustls::{
    crypto::CryptoProvider,
    server::{ClientHello, ResolvesServerCert, WebPkiClientVerifier},
    sign::CertifiedKey,
};

/// The lowest protocol version a listener accepts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TlsVersion {
    #[default]
    Tls12,
    Tls13,
}

/// A certificate chain and its private key, as PEM files.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CertPair {
    pub cert_path: String,
    pub key_path: String,
}

/// Everything a TLS listener needs besides its address.
#[derive(Debug, Clone, PartialEq)]
pub struct TlsOptions {
    /// Served without SNI or when no other certificate matches.
    pub default_cert: CertPair,
    /// More certificates, each selected by the names it lists.
    pub certificates: Vec<CertPair>,
    pub min_version: TlsVersion,
    /// Require client certificates signed by one of these CAs (mTLS).
    pub client_ca_path: Option<String>,
    /// Offer HTTP/2 via ALPN, else HTTP/1.1 only.
    pub enable_h2: bool,
}

impl TlsOptions {
    fn cert_pairs(&self) -> impl Iterator<Item = &CertPair> {
        std::iter::once(&self.default_cert).chain(&self.certificates)
    }
}

fn provider() -> Arc<CryptoProvider> {
    CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
}

/// Load a chain and key and check they belong together.
fn load_pair(pair: &CertPair) -> Result<Arc<CertifiedKey>> {
    let CertPair {
        cert_path,
        key_path,
    } = pair;
    let loaded = load_certs_and_key_files(cert_path, key_path).map_err(|err| {
        eyre!("failed to read certificate '{cert_path}' or key '{key_path}': {err}")
    })?;
    let Some((certs, key)) = loaded else {
        return Err(eyre!(
            "no certificate found in '{cert_path}' or no private key found in '{key_path}' \
             (expected PEM: CERTIFICATE / PRIVATE KEY, RSA PRIVATE KEY or EC PRIVATE KEY)"
        ));
    };
    CertifiedKey::from_der(certs, key, &provider())
        .map(Arc::new)
        .map_err(|err| eyre!("unusable certificate '{cert_path}' / key '{key_path}': {err}"))
}

/// The lowercase DNS names a certificate serves: its SAN DNS names, or its CN if it has none
/// (clients ignore the CN when SANs are present).
fn dns_names(cert: &CertificateDer<'_>) -> Result<Vec<String>> {
    let (_, parsed) = x509_parser::parse_x509_certificate(cert)
        .map_err(|err| eyre!("unparseable certificate: {err}"))?;
    let mut names = Vec::new();
    if let Ok(Some(san)) = parsed.subject_alternative_name() {
        for name in &san.value.general_names {
            if let x509_parser::extensions::GeneralName::DNSName(dns) = name {
                names.push(dns.to_ascii_lowercase());
            }
        }
    }
    if names.is_empty()
        && let Some(cn) = parsed
            .subject()
            .iter_common_name()
            .next()
            .and_then(|cn| cn.as_str().ok())
    {
        names.push(cn.to_ascii_lowercase());
    }
    Ok(names)
}

/// The loaded certificates of a listener, by name.
#[derive(Debug)]
pub struct CertStore {
    exact: HashMap<String, Arc<CertifiedKey>>,
    /// `*.example.com` certificates, keyed by `example.com`.
    wildcard: HashMap<String, Arc<CertifiedKey>>,
    default: Arc<CertifiedKey>,
}

impl CertStore {
    /// Load the default certificate and the others. A name listed by two certificates is an
    /// error, it would be ambiguous which one to serve.
    pub fn load(options: &TlsOptions) -> Result<Self> {
        let mut store = Self {
            exact: HashMap::new(),
            wildcard: HashMap::new(),
            default: load_pair(&options.default_cert)?,
        };
        let mut owner: HashMap<String, &str> = HashMap::new();
        for pair in options.cert_pairs() {
            let is_default = pair == &options.default_cert;
            let key = if is_default {
                store.default.clone()
            } else {
                load_pair(pair)?
            };
            let names = dns_names(&key.cert[0])
                .wrap_err_with(|| format!("certificate '{}'", pair.cert_path))?;
            if names.is_empty() && !is_default {
                return Err(eyre!(
                    "certificate '{}' has no DNS names (SAN or CN) to be selected by",
                    pair.cert_path
                ));
            }
            for name in names {
                if let Some(other) = owner.insert(name.clone(), &pair.cert_path)
                    && other != pair.cert_path
                {
                    return Err(eyre!(
                        "'{name}' is served by both '{other}' and '{}'",
                        pair.cert_path
                    ));
                }
                match name.strip_prefix("*.") {
                    Some(parent) => store.wildcard.insert(parent.to_string(), key.clone()),
                    None => store.exact.insert(name, key.clone()),
                };
            }
        }
        Ok(store)
    }

    /// The certificate for an SNI name: an exact match, else a wildcard for its parent domain
    /// (one label, as in browsers), else the default.
    pub fn pick(&self, sni: Option<&str>) -> Arc<CertifiedKey> {
        let Some(sni) = sni.map(|s| s.trim_end_matches('.').to_ascii_lowercase()) else {
            return self.default.clone();
        };
        self.exact
            .get(&sni)
            .or_else(|| {
                sni.split_once('.')
                    .and_then(|(_, parent)| self.wildcard.get(parent))
            })
            .unwrap_or(&self.default)
            .clone()
    }
}

/// Serves certificates from a [CertStore] that a reload can replace.
#[derive(Debug)]
pub struct CertResolver {
    store: ArcSwap<CertStore>,
}

impl CertResolver {
    pub fn new(store: CertStore) -> Self {
        Self {
            store: ArcSwap::from_pointee(store),
        }
    }

    /// Serve `store` to connections from now on.
    pub fn replace(&self, store: CertStore) {
        self.store.store(Arc::new(store));
    }
}

impl ResolvesServerCert for CertResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.store.load().pick(hello.server_name()))
    }
}

/// Load the CA certificates client certificates must chain to.
fn client_roots(path: &str) -> Result<RootCertStore> {
    let pem = std::fs::read(path).wrap_err_with(|| format!("failed to read client CA '{path}'"))?;
    let mut roots = RootCertStore::empty();
    for block in x509_parser::pem::Pem::iter_from_buffer(&pem) {
        let block = block.map_err(|err| eyre!("client CA '{path}': invalid PEM: {err}"))?;
        if block.label == "CERTIFICATE" {
            roots
                .add(CertificateDer::from(block.contents))
                .map_err(|err| eyre!("client CA '{path}': {err}"))?;
        }
    }
    if roots.is_empty() {
        return Err(eyre!("client CA '{path}' contains no certificate"));
    }
    Ok(roots)
}

/// The listener's server config, its certificates served by `resolver`.
pub fn server_config(options: &TlsOptions, resolver: Arc<CertResolver>) -> Result<ServerConfig> {
    let versions: &[&rustls::SupportedProtocolVersion] = match options.min_version {
        TlsVersion::Tls12 => &[&version::TLS12, &version::TLS13],
        TlsVersion::Tls13 => &[&version::TLS13],
    };
    let builder = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(versions)
        .map_err(|err| eyre!("TLS protocol versions: {err}"))?;
    let builder = match &options.client_ca_path {
        Some(path) => {
            let verifier = WebPkiClientVerifier::builder(Arc::new(client_roots(path)?))
                .build()
                .map_err(|err| eyre!("client CA '{path}': {err}"))?;
            builder.with_client_cert_verifier(verifier)
        }
        None => builder.with_no_client_auth(),
    };
    let mut config = builder.with_cert_resolver(resolver);
    config.alpn_protocols = if options.enable_h2 {
        vec![b"h2".to_vec(), b"http/1.1".to_vec()]
    } else {
        vec![b"http/1.1".to_vec()]
    };
    Ok(config)
}

/// Load everything the listener needs, as building it would, so config validation (and
/// `routini --check`) reports a bad certificate, key, name clash or CA instead of a failure at
/// startup.
pub fn check(options: &TlsOptions) -> Result<()> {
    let store = CertStore::load(options)?;
    server_config(options, Arc::new(CertResolver::new(store)))?;
    Ok(())
}

/// Hash of the certificate and key file contents, to tell on reload whether they changed on
/// disk. `None` if any can't be read.
pub fn cert_fingerprint(options: &TlsOptions) -> Option<u64> {
    let mut hasher = DefaultHasher::new();
    for pair in options.cert_pairs() {
        std::fs::read(&pair.cert_path).ok()?.hash(&mut hasher);
        std::fs::read(&pair.key_path).ok()?.hash(&mut hasher);
    }
    Some(hasher.finish())
}

/// Hash of the client CA file, `None` without one or if it can't be read.
pub fn client_ca_fingerprint(options: &TlsOptions) -> Option<u64> {
    let mut hasher = DefaultHasher::new();
    std::fs::read(options.client_ca_path.as_ref()?)
        .ok()?
        .hash(&mut hasher);
    Some(hasher.finish())
}

/// Self-signed certificates for tests.
#[cfg(test)]
pub(crate) mod test_certs {
    use std::path::{Path, PathBuf};

    use super::*;

    /// Write a self-signed certificate for `names` and its key into `dir` as `<stem>.pem` and
    /// `<stem>.key`.
    pub fn write_named(dir: &Path, stem: &str, names: &[&str]) -> CertPair {
        let names = names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        let cert = rcgen::generate_simple_self_signed(names).unwrap();
        let (cert_path, key_path) = (
            dir.join(format!("{stem}.pem")),
            dir.join(format!("{stem}.key")),
        );
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();
        CertPair {
            cert_path: cert_path.to_string_lossy().into_owned(),
            key_path: key_path.to_string_lossy().into_owned(),
        }
    }

    /// Write a self-signed certificate for 127.0.0.1/localhost and its key into `dir`.
    pub fn write(dir: &Path) -> (String, String) {
        let pair = write_named(dir, "cert", &["127.0.0.1", "localhost"]);
        (pair.cert_path, pair.key_path)
    }

    pub fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("routini-tls-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub fn options(default_cert: CertPair, certificates: Vec<CertPair>) -> TlsOptions {
        TlsOptions {
            default_cert,
            certificates,
            min_version: TlsVersion::Tls12,
            client_ca_path: None,
            enable_h2: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{test_certs::*, *};

    fn pair(cert: &str, key: &str) -> CertPair {
        CertPair {
            cert_path: cert.into(),
            key_path: key.into(),
        }
    }

    fn is(picked: Arc<CertifiedKey>, expected: &CertPair) -> bool {
        let expected = load_pair(expected).unwrap();
        picked.cert[0] == expected.cert[0]
    }

    #[test]
    fn valid_certificate_is_accepted() {
        let (cert, key) = write(&temp_dir("valid"));
        check(&options(pair(&cert, &key), vec![])).unwrap();
    }

    #[test]
    fn missing_files_are_reported() {
        let err = check(&options(
            pair("/nonexistent/c.pem", "/nonexistent/k.pem"),
            vec![],
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("/nonexistent/c.pem"), "{err}");
    }

    #[test]
    fn file_without_key_is_reported() {
        let (cert, _) = write(&temp_dir("nokey"));
        let err = check(&options(pair(&cert, &cert), vec![]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no private key"), "{err}");
    }

    #[test]
    fn key_of_another_certificate_is_rejected() {
        let (cert_a, _) = write(&temp_dir("a"));
        let (_, key_b) = write(&temp_dir("b"));
        let err = check(&options(pair(&cert_a, &key_b), vec![]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unusable certificate"), "{err}");
    }

    #[test]
    fn certificates_are_picked_by_sni() {
        let dir = temp_dir("sni");
        let default = write_named(&dir, "default", &["default.test"]);
        let api = write_named(&dir, "api", &["api.test", "API2.test"]);
        let wild = write_named(&dir, "wild", &["*.apps.test"]);
        let store =
            CertStore::load(&options(default.clone(), vec![api.clone(), wild.clone()])).unwrap();

        assert!(is(store.pick(None), &default));
        assert!(is(store.pick(Some("other.test")), &default));
        assert!(is(store.pick(Some("default.test")), &default));
        assert!(is(store.pick(Some("api.test")), &api));
        assert!(
            is(store.pick(Some("Api2.Test.")), &api),
            "case and trailing dot"
        );
        assert!(is(store.pick(Some("one.apps.test")), &wild));
        assert!(
            is(store.pick(Some("apps.test")), &default),
            "wildcard needs a label"
        );
        assert!(
            is(store.pick(Some("a.b.apps.test")), &default),
            "one label only"
        );
    }

    #[test]
    fn a_name_served_by_two_certificates_is_rejected() {
        let dir = temp_dir("clash");
        let default = write_named(&dir, "default", &["default.test"]);
        let a = write_named(&dir, "a", &["api.test"]);
        let b = write_named(&dir, "b", &["api.test"]);
        let err = CertStore::load(&options(default, vec![a, b]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("'api.test' is served by both"), "{err}");
    }

    #[test]
    fn client_ca_problems_are_reported() {
        let dir = temp_dir("ca");
        let (cert, key) = write(&dir);
        let mut opts = options(pair(&cert, &key), vec![]);
        opts.client_ca_path = Some(key.clone());
        let err = check(&opts).unwrap_err().to_string();
        assert!(err.contains("contains no certificate"), "{err}");
        opts.client_ca_path = Some(cert);
        check(&opts).unwrap();
    }

    #[test]
    fn fingerprint_changes_with_file_contents() {
        let dir = temp_dir("fingerprint");
        let (cert, key) = write(&dir);
        let opts = options(pair(&cert, &key), vec![]);
        let before = cert_fingerprint(&opts).unwrap();
        assert_eq!(cert_fingerprint(&opts), Some(before));
        write(&dir);
        assert_ne!(cert_fingerprint(&opts), Some(before));
        assert_eq!(
            cert_fingerprint(&options(pair("/nonexistent", "/nonexistent"), vec![])),
            None
        );
    }
}
