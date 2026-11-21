//! End-to-end tests of nginx-style route and server features, configured through TOML.
use reqwest::StatusCode;
use routini::{config::Config, utils::config_loader::ConfigFormat};

use crate::{
    config_app::{Ports, ReloadApp, route},
    helpers::TestApp,
};

/// A valid config for `routes`, with `server_extra` inserted into its `[server]` table.
fn config(ports: &Ports, server_extra: &str, routes: &str) -> Config {
    let toml = ports
        .toml(routes)
        .replacen("[server]\n", &format!("[server]\n{server_extra}\n"), 1);
    let config = Config::parse(&toml, ConfigFormat::Toml).unwrap();
    config.validate().unwrap();
    config
}

#[tokio::test]
async fn client_ip_is_taken_from_trusted_proxies() {
    let b = TestApp::create_backends(1).await.unwrap();
    let ports = Ports::new();
    let routes = route(
        "/a/*",
        &[&b[0]],
        "[proxy.router.access]\ndeny = [\"198.51.100.0/24\"]",
        "",
    );
    let config = config(
        &ports,
        "real_ip = { trusted_proxies = [\"127.0.0.1\"] }",
        &routes,
    );
    let app = ReloadApp::start(config, None).await;

    let headers_for = |xff: &'static str| {
        let req = app.client.get(format!("{}/a/headers", app.base));
        async move {
            let res = req.header("x-forwarded-for", xff).send().await.unwrap();
            (res.status(), res.text().await.unwrap())
        }
    };

    // The test client is the trusted proxy (127.0.0.1); the address it forwarded is the client.
    // A value the client spoofed in front of it is ignored, and the backend sees only the client.
    let (status, body) = headers_for("6.6.6.6, 203.0.113.1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "proto=http;for=203.0.113.1");

    // allow/deny applies to the client, not the proxy
    let (status, _) = headers_for("198.51.100.20").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A chain from the trusted proxy that can't be read is rejected rather than guessed at
    let (status, _) = headers_for("198.51.100.20, garbage").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = headers_for("garbage, 203.0.113.1").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "garbage the client wrote left of it is ignored"
    );

    // Without the header the proxy itself is the client
    let (status, body) = app.get("/a/headers").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "proto=http;for=127.0.0.1");
}

#[tokio::test]
async fn forwarded_header_is_ignored_from_untrusted_peers() {
    let b = TestApp::create_backends(1).await.unwrap();
    let ports = Ports::new();
    let routes = route(
        "/a/*",
        &[&b[0]],
        "[proxy.router.access]\ndeny = [\"198.51.100.0/24\"]",
        "",
    );
    let config = config(
        &ports,
        "real_ip = { trusted_proxies = [\"10.0.0.0/8\"] }",
        &routes,
    );
    let app = ReloadApp::start(config, None).await;

    let res = app
        .client
        .get(format!("{}/a/headers", app.base))
        .header("x-forwarded-for", "198.51.100.20")
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "a spoofed header must not get anyone denied"
    );
    assert_eq!(res.text().await.unwrap(), "proto=http;for=127.0.0.1");
}

/// The backend that served each of `users`, sending the user id the way `send` attaches it.
async fn backends_per_user(
    app: &ReloadApp,
    users: impl Iterator<Item = String>,
    send: impl Fn(reqwest::RequestBuilder, &str) -> reqwest::RequestBuilder,
) -> Vec<String> {
    let mut served = Vec::new();
    for user in users {
        let req = app.client.get(format!("{}/a/health", app.base));
        let res = send(req, &user).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        served.push(res.text().await.unwrap());
    }
    served
}

#[tokio::test]
async fn hash_strategies_are_sticky_per_hash_key() {
    let b = TestApp::create_backends(4).await.unwrap();
    let backends: Vec<_> = b.iter().collect();
    let users = || (0..40).map(|i| format!("user-{i}"));
    let by_header = |req: reqwest::RequestBuilder, user: &str| req.header("x-user", user);
    let lb = |strategy: &str, key: &str| format!("strategy = \"{strategy}\"\nhash_key = \"{key}\"");

    for strategy in ["FNVHash", "Consistent"] {
        let ports = Ports::new();
        let lb_header = lb(strategy, "header:X-User");
        let app = ReloadApp::start(
            ports_config(&ports, &route("/a/*", &backends, "", &lb_header)),
            None,
        )
        .await;

        let first = backends_per_user(&app, users(), by_header).await;
        let again = backends_per_user(&app, users(), by_header).await;
        assert_eq!(first, again, "{strategy}: each user keeps its backend");
        let distinct: std::collections::HashSet<_> = first.iter().collect();
        assert!(
            distinct.len() >= 2,
            "{strategy}: users spread over backends, got {distinct:?}"
        );

        // Switching the key to a cookie applies on reload
        let lb_cookie = lb(strategy, "cookie:sid");
        app.manager
            .reload(ports_config(
                &ports,
                &route("/a/*", &backends, "", &lb_cookie),
            ))
            .await
            .unwrap();
        let by_cookie = |req: reqwest::RequestBuilder, user: &str| {
            req.header("cookie", format!("theme=x; sid={user}"))
        };
        let cookie = backends_per_user(&app, users(), by_cookie).await;
        assert_eq!(cookie, backends_per_user(&app, users(), by_cookie).await);
        assert!(
            cookie
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                >= 2
        );
        // Same value hashed, whether it came from the header or the cookie
        assert_eq!(cookie, first, "{strategy}");
    }
}

fn ports_config(ports: &Ports, routes: &str) -> Config {
    config(ports, "", routes)
}
