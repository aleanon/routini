//! TLS termination end to end: a real server with a self-signed certificate, real backends.
use std::{path::PathBuf, sync::Arc, time::Duration};

use reqwest::{Certificate, StatusCode, Version, redirect::Policy};
use routini::{reload::RouteManager, utils::config_loader::load_config_from};

use crate::helpers::TestApp;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Write a self-signed certificate for 127.0.0.1 into `dir`, returning its PEM.
fn write_cert(dir: &PathBuf) -> String {
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    std::fs::write(dir.join("cert.pem"), cert.cert.pem()).unwrap();
    std::fs::write(dir.join("key.pem"), cert.key_pair.serialize_pem()).unwrap();
    cert.cert.pem()
}

struct Options {
    h2: bool,
    redirect: bool,
    trusted_proxy: bool,
    /// Appended to the `[proxy.tls]` table.
    tls_extra: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            h2: true,
            redirect: true,
            trusted_proxy: false,
            tls_extra: String::new(),
        }
    }
}

struct TlsApp {
    tls_port: u16,
    https: String,
    plain: String,
    cert_pem: String,
    dir: PathBuf,
    manager: Arc<RouteManager>,
}

impl TlsApp {
    async fn start(name: &str, backend: &str, opts: Options) -> Self {
        let dir =
            std::env::temp_dir().join(format!("routini-tls-it-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert_pem = write_cert(&dir);

        let (plain_port, tls_port, metrics_port) = (free_port(), free_port(), free_port());
        // Relative cert paths are resolved against the config file's directory
        let toml = format!(
            "[server]\nprometheus_address = \"127.0.0.1:{metrics_port}\"\n\
             grace_period_seconds = 0\nhttps_redirect = {redirect}\n\n\
             [proxy]\nlistener = \"127.0.0.1:{plain_port}\"\n\n\
             [proxy.tls]\nlistener = \"127.0.0.1:{tls_port}\"\ncert_path = \"cert.pem\"\n\
             key_path = \"key.pem\"\nenable_h2 = {h2}\n{tls_extra}\n\n\
             [[proxy.router]]\npath = \"/a/*\"\n\
             [proxy.router.headers]\ntrusted_proxy = {trusted}\n\
             [proxy.router.load_balancer]\nhealth_check_interval_secs = 0\n\
             [[proxy.router.load_balancer.upstreams]]\naddress = \"{backend}\"\n",
            redirect = opts.redirect,
            h2 = opts.h2,
            trusted = opts.trusted_proxy,
            tls_extra = opts.tls_extra,
        );
        let file = dir.join("config.toml");
        std::fs::write(&file, toml).unwrap();

        let (server, manager) = load_config_from(&file)
            .unwrap()
            .into_server_builder()
            .unwrap()
            .reload_on_sighup(file.to_string_lossy().into_owned())
            .build_with_reload_handle();
        std::thread::spawn(move || server.run_forever());

        let app = Self {
            tls_port,
            https: format!("https://127.0.0.1:{tls_port}"),
            plain: format!("http://127.0.0.1:{plain_port}"),
            cert_pem,
            dir,
            manager,
        };
        // A TCP connect, so listeners that require client certificates are probed too
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(("127.0.0.1", tls_port))
                .await
                .is_ok()
                && app.manager.is_running().await
            {
                return app;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("TLS server did not start");
    }

    /// A client trusting the self-signed certificate.
    fn client(&self, http1_only: bool) -> reqwest::Client {
        let mut builder = reqwest::Client::builder()
            .add_root_certificate(Certificate::from_pem(self.cert_pem.as_bytes()).unwrap());
        if http1_only {
            builder = builder.http1_only();
        }
        builder.build().unwrap()
    }

    async fn get_body(&self, client: &reqwest::Client, url: String) -> String {
        client.get(url).send().await.unwrap().text().await.unwrap()
    }
}

impl Drop for TlsApp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn https_serves_routes_over_http2_and_http1() {
    let b = TestApp::create_backends(1).await.unwrap();
    let app = TlsApp::start("h2", &b[0], Options::default()).await;

    let res = app
        .client(false)
        .get(format!("{}/a/health", app.https))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.version(), Version::HTTP_2, "ALPN should negotiate h2");
    assert_eq!(res.text().await.unwrap(), b[0]);

    let res = app
        .client(true)
        .get(format!("{}/a/health", app.https))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.version(), Version::HTTP_11);
}

#[tokio::test]
async fn http2_can_be_disabled() {
    let b = TestApp::create_backends(1).await.unwrap();
    let opts = Options {
        h2: false,
        ..Default::default()
    };
    let app = TlsApp::start("h1", &b[0], opts).await;
    let res = app
        .client(false)
        .get(format!("{}/a/health", app.https))
        .send()
        .await
        .unwrap();
    assert_eq!(res.version(), Version::HTTP_11);
}

#[tokio::test]
async fn untrusted_and_plaintext_clients_are_refused() {
    let b = TestApp::create_backends(1).await.unwrap();
    let app = TlsApp::start("refuse", &b[0], Options::default()).await;

    // Self-signed certificate not in the default trust store
    assert!(
        reqwest::get(format!("{}/a/health", app.https))
            .await
            .is_err()
    );

    // Plain HTTP to the TLS port
    let plain_to_tls = app.https.replacen("https://", "http://", 1);
    let res = reqwest::get(format!("{plain_to_tls}/a/health")).await;
    assert!(res.is_err() || !res.unwrap().status().is_success());
}

#[tokio::test]
async fn backends_receive_forwarded_headers() {
    let b = TestApp::create_backends(1).await.unwrap();

    // Default: client-supplied X-Forwarded-For is discarded (spoofing), proto is overwritten
    let app = TlsApp::start("fwd", &b[0], Options::default()).await;
    let body = app
        .client(false)
        .get(format!("{}/a/headers", app.https))
        .header("X-Forwarded-Proto", "http")
        .header("X-Forwarded-For", "203.0.113.7")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "proto=https;for=127.0.0.1");

    // Behind a trusted proxy the chain is kept and the client IP appended
    let opts = Options {
        trusted_proxy: true,
        ..Default::default()
    };
    let trusted = TlsApp::start("fwd-trusted", &b[0], opts).await;
    let body = trusted
        .client(false)
        .get(format!("{}/a/headers", trusted.https))
        .header("X-Forwarded-For", "203.0.113.7")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "proto=https;for=203.0.113.7, 127.0.0.1");
}

#[tokio::test]
async fn plain_listener_without_redirect_forwards_proto_http() {
    let b = TestApp::create_backends(1).await.unwrap();
    let opts = Options {
        redirect: false,
        ..Default::default()
    };
    let app = TlsApp::start("plain", &b[0], opts).await;
    let body = app
        .get_body(&reqwest::Client::new(), format!("{}/a/headers", app.plain))
        .await;
    assert_eq!(body, "proto=http;for=127.0.0.1");
}

#[tokio::test]
async fn plain_http_is_redirected_to_the_tls_listener() {
    let b = TestApp::create_backends(1).await.unwrap();
    let app = TlsApp::start("redirect", &b[0], Options::default()).await;

    let no_follow = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();
    let res = no_follow
        .post(format!("{}/a/health?x=1&y=2", app.plain))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::PERMANENT_REDIRECT);
    // The plain listener's port is replaced with the TLS listener's
    assert_eq!(
        res.headers()["location"],
        format!("{}/a/health?x=1&y=2", app.https).as_str()
    );

    // Following it ends up at the backend over TLS
    let res = app
        .client(false)
        .get(format!("{}/a/health", app.plain))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.url().scheme(), "https");
    assert_eq!(res.text().await.unwrap(), b[0]);
}

/// A self-signed certificate for `names`, written to `<dir>/<stem>.pem` and `.key`. Returns
/// the certificate PEM.
fn write_named(dir: &std::path::Path, stem: &str, names: &[&str]) -> String {
    let names = names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
    let cert = rcgen::generate_simple_self_signed(names).unwrap();
    std::fs::write(dir.join(format!("{stem}.pem")), cert.cert.pem()).unwrap();
    std::fs::write(
        dir.join(format!("{stem}.key")),
        cert.key_pair.serialize_pem(),
    )
    .unwrap();
    cert.cert.pem()
}

/// A fresh client (new connections) for `https://<name>:<port>` that trusts only `cert_pem`,
/// so a handshake succeeds only if the server presents that certificate.
fn client_trusting(app: &TlsApp, name: &str, cert_pem: &str) -> (reqwest::Client, String) {
    let client = reqwest::Client::builder()
        .tls_built_in_root_certs(false)
        .add_root_certificate(Certificate::from_pem(cert_pem.as_bytes()).unwrap())
        .resolve(name, ([127, 0, 0, 1], app.tls_port).into())
        .build()
        .unwrap();
    (client, format!("https://{name}:{}/a/health", app.tls_port))
}

async fn handshake_ok(app: &TlsApp, name: &str, cert_pem: &str) -> bool {
    let (client, url) = client_trusting(app, name, cert_pem);
    client.get(url).send().await.is_ok()
}

#[tokio::test]
async fn certificates_are_selected_by_sni() {
    let b = TestApp::create_backends(1).await.unwrap();
    // The app writes its default certificate for 127.0.0.1 into its dir; the others are
    // written there before it starts (relative paths resolve against the config dir)
    let dir = std::env::temp_dir().join(format!("routini-tls-it-sni-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let api = write_named(&dir, "api", &["api.test"]);
    let apps = write_named(&dir, "apps", &["*.apps.test"]);
    let opts = Options {
        tls_extra: "[[proxy.tls.certificates]]\ncert_path = \"api.pem\"\nkey_path = \"api.key\"\n\
                    [[proxy.tls.certificates]]\ncert_path = \"apps.pem\"\nkey_path = \"apps.key\""
            .into(),
        ..Default::default()
    };
    let app = TlsApp::start("sni", &b[0], opts).await;

    assert!(handshake_ok(&app, "api.test", &api).await);
    assert!(handshake_ok(&app, "one.apps.test", &apps).await);
    // Unknown names get the default certificate, not api.test's
    assert!(!handshake_ok(&app, "other.test", &api).await);
    let (client, _) = client_trusting(&app, "other.test", &app.cert_pem);
    let res = client.get(format!("{}/a/health", app.https)).send().await;
    assert!(res.is_ok(), "no SNI for an IP: the default certificate");
}

#[tokio::test]
async fn rotated_certificates_are_served_after_a_reload() {
    let b = TestApp::create_backends(1).await.unwrap();
    let app = TlsApp::start("rotate", &b[0], Options::default()).await;
    let old = app.cert_pem.clone();

    let report = app.manager.reload_from_file().await.unwrap();
    assert!(report.restart_required.is_empty(), "{report:?}");
    assert!(
        !report
            .updated
            .contains(&"proxy.tls certificates".to_string())
    );

    // Same paths, new contents
    let new = write_cert(&app.dir);
    let report = app.manager.reload_from_file().await.unwrap();
    assert!(report.restart_required.is_empty(), "{report:?}");
    assert!(
        report
            .updated
            .contains(&"proxy.tls certificates".to_string()),
        "{report:?}"
    );

    let (client, url) = client_trusting(&app, "127.0.0.1", &new);
    assert_eq!(
        client.get(url).send().await.unwrap().status(),
        StatusCode::OK
    );
    let (client, url) = client_trusting(&app, "127.0.0.1", &old);
    assert!(
        client.get(url).send().await.is_err(),
        "old certificate no longer served"
    );
}

#[tokio::test]
async fn a_broken_certificate_fails_the_reload_and_keeps_serving() {
    let b = TestApp::create_backends(1).await.unwrap();
    let app = TlsApp::start("broken", &b[0], Options::default()).await;
    std::fs::write(app.dir.join("cert.pem"), "not a certificate").unwrap();
    assert!(app.manager.reload_from_file().await.is_err());
    let res = app
        .client(false)
        .get(format!("{}/a/health", app.https))
        .send()
        .await;
    assert_eq!(res.unwrap().status(), StatusCode::OK);
}

#[tokio::test]
async fn min_version_refuses_older_clients() {
    let b = TestApp::create_backends(1).await.unwrap();
    let opts = Options {
        tls_extra: "min_version = \"1.3\"".into(),
        ..Default::default()
    };
    let app = TlsApp::start("tls13", &b[0], opts).await;
    let tls12_only = reqwest::Client::builder()
        .add_root_certificate(Certificate::from_pem(app.cert_pem.as_bytes()).unwrap())
        .max_tls_version(reqwest::tls::Version::TLS_1_2)
        .build()
        .unwrap();
    assert!(
        tls12_only
            .get(format!("{}/a/health", app.https))
            .send()
            .await
            .is_err()
    );
    let res = app
        .client(false)
        .get(format!("{}/a/health", app.https))
        .send()
        .await;
    assert_eq!(res.unwrap().status(), StatusCode::OK);
}

/// Whether this process has a thread whose name starts with `prefix` (Linux truncates names to
/// 15 bytes).
fn has_thread(prefix: &str) -> bool {
    std::fs::read_dir("/proc/self/task").unwrap().any(|task| {
        std::fs::read_to_string(task.unwrap().path().join("comm"))
            .is_ok_and(|name| name.starts_with(prefix))
    })
}

#[tokio::test]
async fn handshakes_can_run_on_dedicated_threads() {
    const OFFLOAD_THREAD: &str = "downstream TLS";
    let b = TestApp::create_backends(1).await.unwrap();
    let opts = Options {
        tls_extra: "handshake_offload = { pools = 1, threads_per_pool = 2 }".into(),
        ..Default::default()
    };
    let app = TlsApp::start("offload", &b[0], opts).await;
    for _ in 0..3 {
        // A new client each time, so each request takes a handshake
        let res = app
            .client(false)
            .get(format!("{}/a/health", app.https))
            .send()
            .await;
        assert_eq!(res.unwrap().status(), StatusCode::OK);
    }
    // The pool starts with the first offloaded handshake
    assert!(has_thread(OFFLOAD_THREAD));
}

#[tokio::test]
async fn client_certificates_are_required_with_a_client_ca() {
    use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair};
    let b = TestApp::create_backends(1).await.unwrap();

    let dir = std::env::temp_dir().join(format!("routini-tls-it-mtls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let issue = |ca: Option<(&rcgen::Certificate, &KeyPair)>| {
        let mut params = CertificateParams::new(vec!["client".to_string()]).unwrap();
        let key = KeyPair::generate().unwrap();
        let cert = match ca {
            Some((ca, ca_key)) => {
                params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
                params.signed_by(&key, ca, ca_key).unwrap()
            }
            None => {
                params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
                params.self_signed(&key).unwrap()
            }
        };
        (cert, key)
    };
    let (ca, ca_key) = issue(None);
    std::fs::write(dir.join("clients.pem"), ca.pem()).unwrap();
    let (client_cert, client_key) = issue(Some((&ca, &ca_key)));
    let (other_ca, other_key) = issue(None);
    let (stranger, stranger_key) = issue(Some((&other_ca, &other_key)));

    let opts = Options {
        tls_extra: format!("client_ca_path = \"{}\"", dir.join("clients.pem").display()),
        ..Default::default()
    };
    let app = TlsApp::start("mtls", &b[0], opts).await;
    let with_identity = |cert: &rcgen::Certificate, key: &KeyPair| {
        let identity = reqwest::Identity::from_pkcs8_pem(
            cert.pem().as_bytes(),
            key.serialize_pem().as_bytes(),
        )
        .unwrap();
        reqwest::Client::builder()
            .add_root_certificate(Certificate::from_pem(app.cert_pem.as_bytes()).unwrap())
            .identity(identity)
            .build()
            .unwrap()
    };
    let url = format!("{}/a/health", app.https);

    let res = with_identity(&client_cert, &client_key)
        .get(&url)
        .send()
        .await;
    assert_eq!(res.unwrap().status(), StatusCode::OK);
    assert!(
        app.client(false).get(&url).send().await.is_err(),
        "no client certificate"
    );
    assert!(
        with_identity(&stranger, &stranger_key)
            .get(&url)
            .send()
            .await
            .is_err(),
        "certificate from another CA"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
