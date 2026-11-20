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
}

impl Default for Options {
    fn default() -> Self {
        Self {
            h2: true,
            redirect: true,
            trusted_proxy: false,
        }
    }
}

struct TlsApp {
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
             key_path = \"key.pem\"\nenable_h2 = {h2}\n\n\
             [[proxy.router]]\npath = \"/a/*\"\n\
             [proxy.router.headers]\ntrusted_proxy = {trusted}\n\
             [proxy.router.load_balancer]\nhealth_check_interval_secs = 0\n\
             [[proxy.router.load_balancer.upstreams]]\naddress = \"{backend}\"\n",
            redirect = opts.redirect,
            h2 = opts.h2,
            trusted = opts.trusted_proxy,
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
            https: format!("https://127.0.0.1:{tls_port}"),
            plain: format!("http://127.0.0.1:{plain_port}"),
            cert_pem,
            dir,
            manager,
        };
        let client = app.client(false);
        for _ in 0..100 {
            if client.get(&app.https).send().await.is_ok() {
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

#[tokio::test]
async fn reload_reports_rotated_certificates_as_needing_an_upgrade() {
    let b = TestApp::create_backends(1).await.unwrap();
    let app = TlsApp::start("rotate", &b[0], Options::default()).await;

    let report = app.manager.reload_from_file().await.unwrap();
    assert!(report.restart_required.is_empty(), "{report:?}");

    write_cert(&app.dir);
    let report = app.manager.reload_from_file().await.unwrap();
    assert_eq!(
        report.restart_required,
        vec!["proxy.tls certificate files changed on disk"]
    );

    // Still serving the certificate it started with
    let res = app
        .client(false)
        .get(format!("{}/a/health", app.https))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}
