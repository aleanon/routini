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

#[tokio::test]
async fn basic_auth_accepts_bcrypt_hashes() {
    let b = TestApp::create_backends(1).await.unwrap();
    let hash = bcrypt::hash("s3cret", 4).unwrap();
    let access =
        format!("[proxy.router.access]\nbasic_auth_realm = \"ops\"\nbasic_auth = [\"bob:{hash}\"]");
    let app = ReloadApp::start(
        ports_config(&Ports::new(), &route("/a/*", &[&b[0]], &access, "")),
        None,
    )
    .await;

    let get = |user: &'static str, password: &'static str| {
        app.client
            .get(format!("{}/a/health", app.base))
            .basic_auth(user, Some(password))
            .send()
    };
    assert_eq!(get("bob", "s3cret").await.unwrap().status(), StatusCode::OK);
    let denied = get("bob", "wrong").await.unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(denied.headers()["www-authenticate"], "Basic realm=\"ops\"");
    let (status, _) = app.get("/a/health").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// A backend that accepts TCP connections and closes them without answering: healthy to a TCP
/// check, broken for HTTP.
async fn tcp_only_backend() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });
    address
}

#[tokio::test]
async fn http_health_check_takes_out_backends_tcp_would_keep() {
    let b = TestApp::create_backends(1).await.unwrap();
    let broken = tcp_only_backend().await;
    let ports = Ports::new();
    let config_with = |check: &str| {
        let routes = route("/a/*", &[&b[0], &broken], "", check).replace(
            "health_check_interval_secs = 0",
            "health_check_interval_secs = 1",
        );
        ports_config(&ports, &routes)
    };
    // TCP check (the default). The broken backend accepts connections, so connect-error
    // retries don't save the requests sent to it
    let app = ReloadApp::start(config_with(""), None).await;

    let statuses = |app: &ReloadApp| {
        let (client, url) = (app.client.clone(), format!("{}/a/health", app.base));
        async move {
            let mut ok = 0;
            for _ in 0..20 {
                if client.get(&url).send().await.unwrap().status() == StatusCode::OK {
                    ok += 1;
                }
            }
            ok
        }
    };
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert!(
        statuses(&app).await < 20,
        "TCP check keeps the broken backend"
    );

    let report = app
        .manager
        .reload(config_with(
            "[proxy.router.load_balancer.health_check]\ntype = \"http\"\npath = \"/health\"",
        ))
        .await
        .unwrap();
    assert_eq!(
        report.rebuilt,
        vec!["/a/*"],
        "a changed check rebuilds the route"
    );
    // The rebuilt route probes before its backends are trusted again
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(statuses(&app).await, 20);
}

#[tokio::test]
async fn rate_limit_allows_a_burst_per_client_then_rejects_or_delays() {
    let b = TestApp::create_backends(1).await.unwrap();
    let ports = Ports::new();
    let routes = route(
        "/a/*",
        &[&b[0]],
        "rate_limit_rps = 1\nrate_limit_burst = 2",
        "",
    ) + &route(
        "/d/*",
        &[&b[0]],
        "rate_limit_rps = 10\nrate_limit_burst = 2\nrate_limit_delay = true",
        "",
    );
    let app = ReloadApp::start(
        config(
            &ports,
            "real_ip = { trusted_proxies = [\"127.0.0.1\"] }",
            &routes,
        ),
        None,
    )
    .await;
    let get = |path: &str, client: &str| {
        app.client
            .get(format!("{}{path}", app.base))
            .header("x-forwarded-for", client.to_string())
            .send()
    };

    // nodelay: the burst (1 + 2) passes, the next is rejected; another client is unaffected
    for _ in 0..3 {
        assert_eq!(
            get("/a/health", "203.0.113.1").await.unwrap().status(),
            StatusCode::OK
        );
    }
    assert_eq!(
        get("/a/health", "203.0.113.1").await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        get("/a/health", "203.0.113.2").await.unwrap().status(),
        StatusCode::OK
    );

    // delay: the burst is held back to 10/s instead of passing at once
    let start = std::time::Instant::now();
    for _ in 0..3 {
        assert_eq!(
            get("/d/health", "203.0.113.3").await.unwrap().status(),
            StatusCode::OK
        );
    }
    assert!(
        start.elapsed() >= std::time::Duration::from_millis(190),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(
        get("/d/health", "203.0.113.4").await.unwrap().status(),
        StatusCode::OK
    );
}

/// Send one HTTP/1.1 request on `stream` and read the whole response (the worker sets
/// Content-Length). Returns whether the server announced it keeps the connection open.
async fn exchange(stream: &mut tokio::net::TcpStream, extra_header: &str) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let request = format!("GET /a/health HTTP/1.1\r\nHost: test\r\n{extra_header}\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    let mut buf = [0; 4096];
    loop {
        let n = stream.read(&mut buf).await.unwrap();
        assert!(n > 0, "connection closed before the response was complete");
        response.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&response).to_lowercase();
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let length: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map(|v| v.trim().parse().unwrap())
                .unwrap_or(0);
            if body.len() >= length {
                assert!(head.starts_with("http/1.1 200"), "{head}");
                return !head.contains("connection: close");
            }
        }
    }
}

/// Whether the server closes `stream` within `within`.
async fn closed_within(stream: &mut tokio::net::TcpStream, within: std::time::Duration) -> bool {
    use tokio::io::AsyncReadExt;
    let mut buf = [0; 1];
    matches!(
        tokio::time::timeout(within, stream.read(&mut buf)).await,
        Ok(Ok(0)) | Ok(Err(_))
    )
}

#[tokio::test]
async fn idle_client_connections_are_closed_after_the_keepalive_timeout() {
    use std::time::Duration;
    let b = TestApp::create_backends(1).await.unwrap();
    let ports = Ports::new();
    let app = ReloadApp::start(
        config(
            &ports,
            "keepalive_timeout_secs = 1",
            &route("/a/*", &[&b[0]], "", ""),
        ),
        None,
    )
    .await;
    let address = app.base.trim_start_matches("http://").to_string();
    let connect = || tokio::net::TcpStream::connect(address.clone());

    // Kept alive for another request, then closed once idle for the timeout
    let mut stream = connect().await.unwrap();
    assert!(exchange(&mut stream, "").await);
    assert!(exchange(&mut stream, "").await, "still open for reuse");
    assert!(!closed_within(&mut stream, Duration::from_millis(500)).await);
    assert!(closed_within(&mut stream, Duration::from_secs(2)).await);

    // A client asking to close is not kept alive by the timeout
    let mut stream = connect().await.unwrap();
    assert!(!exchange(&mut stream, "Connection: close\r\n").await);
    assert!(closed_within(&mut stream, Duration::from_millis(500)).await);

    // 0 disables keepalive
    let ports = Ports::new();
    let app = ReloadApp::start(
        config(
            &ports,
            "keepalive_timeout_secs = 0",
            &route("/a/*", &[&b[0]], "", ""),
        ),
        None,
    )
    .await;
    let mut stream = tokio::net::TcpStream::connect(app.base.trim_start_matches("http://"))
        .await
        .unwrap();
    assert!(!exchange(&mut stream, "").await);
    assert!(closed_within(&mut stream, Duration::from_millis(500)).await);
}

/// A backend that reads each request header and then sends `response`, or, for `None`, never
/// answers. Returns its address.
async fn scripted_backend(response: Option<&'static str>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                match response {
                    Some(response) => {
                        let _ = stream.write_all(response.as_bytes()).await;
                    }
                    None => tokio::time::sleep(std::time::Duration::from_secs(30)).await,
                }
            });
        }
    });
    address
}

const UNAVAILABLE: &str =
    "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 4\r\nconnection: close\r\n\r\ndown";

/// Statuses of `n` requests with `method` to `path`.
async fn statuses(app: &ReloadApp, method: reqwest::Method, path: &str, n: usize) -> Vec<u16> {
    let mut out = Vec::new();
    for _ in 0..n {
        let res = app
            .client
            .request(method.clone(), format!("{}{path}", app.base))
            .send()
            .await
            .unwrap();
        out.push(res.status().as_u16());
    }
    out
}

#[tokio::test]
async fn configured_statuses_are_retried_on_another_backend() {
    let b = TestApp::create_backends(1).await.unwrap();
    let down = scripted_backend(Some(UNAVAILABLE)).await;
    let ports = Ports::new();
    let routes = route(
        "/a/*",
        &[&b[0], &down],
        "[proxy.router.retry]\nretry_on_status = [503]",
        "",
    ) + &route("/plain/*", &[&b[0], &down], "", "");
    let app = ReloadApp::start(config(&ports, "", &routes), None).await;

    assert_eq!(
        statuses(&app, reqwest::Method::GET, "/a/health", 10).await,
        [200; 10]
    );
    // Without retry_on_status the 503 reaches the client
    assert!(
        statuses(&app, reqwest::Method::GET, "/plain/health", 10)
            .await
            .contains(&503)
    );
    // POST isn't resent after it reached a backend (the worker answers it 405)
    let post = statuses(&app, reqwest::Method::POST, "/a/health", 10).await;
    assert!(post.contains(&503), "{post:?}");
}

#[tokio::test]
async fn timeouts_are_retried_on_another_backend() {
    let b = TestApp::create_backends(1).await.unwrap();
    let hangs = scripted_backend(None).await;
    let ports = Ports::new();
    let timeouts = "[proxy.router.timeouts]\nread_ms = 200";
    let routes = route("/a/*", &[&b[0], &hangs], timeouts, "")
        + &route(
            "/off/*",
            &[&b[0], &hangs],
            &format!("{timeouts}\n[proxy.router.retry]\nretry_on_timeout = false"),
            "",
        );
    let app = ReloadApp::start(config(&ports, "", &routes), None).await;

    assert_eq!(
        statuses(&app, reqwest::Method::GET, "/a/health", 6).await,
        [200; 6]
    );
    let off = statuses(&app, reqwest::Method::GET, "/off/health", 6).await;
    assert!(off.iter().any(|s| *s >= 500), "{off:?}");
}

/// Answers the first request on each connection (keeping it alive) and closes the connection
/// when a second request arrives on it, like a backend whose keep-alive just expired.
async fn one_request_per_connection_backend() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0; 4096];
                if stream.read(&mut buf).await.unwrap_or(0) == 0 {
                    return;
                }
                let ok = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
                let _ = stream.write_all(ok.as_bytes()).await;
                // The next request on this connection: close without answering
                let _ = stream.read(&mut buf).await;
            });
        }
    });
    address
}

#[tokio::test]
async fn a_lone_backend_is_retried_when_its_pooled_connection_went_stale() {
    // Every second request goes out on a pooled connection the backend then drops. The retry
    // has to be allowed to go to the same, only, backend again.
    let flaky = one_request_per_connection_backend().await;
    let ports = Ports::new();
    let app = ReloadApp::start(config(&ports, "", &route("/a/*", &[&flaky], "", "")), None).await;
    assert_eq!(
        statuses(&app, reqwest::Method::GET, "/a/x", 6).await,
        [200; 6]
    );
}

#[tokio::test]
async fn upstreams_can_be_hostnames() {
    let b = TestApp::create_backends(1).await.unwrap();
    let port = b[0].rsplit_once(':').unwrap().1;
    let named = format!("localhost:{port}");
    let ports = Ports::new();
    let app = ReloadApp::start(config(&ports, "", &route("/a/*", &[&named], "", "")), None).await;
    // localhost may also resolve to ::1, where the worker doesn't listen: connect errors fail
    // over to 127.0.0.1
    let (status, body) = app.get("/a/health").await;
    assert_eq!((status, body), (StatusCode::OK, b[0].clone()));
}

#[tokio::test]
async fn a_slow_client_does_not_get_the_backend_ejected() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let b = TestApp::create_backends(1).await.unwrap();
    let ports = Ports::new();
    // Ejects the only backend on its first failure, if the timeout were counted against it
    let routes = route(
        "/a/*",
        &[&b[0]],
        "[proxy.router.passive_health]\nenabled = true\nmax_fails = 1\nfail_timeout_secs = 60",
        "",
    );
    let app = ReloadApp::start(
        config(&ports, "client_body_timeout_secs = 1", &routes),
        None,
    )
    .await;

    // Announce a body and stall after two bytes, until the client body timeout fires
    let mut stream = tokio::net::TcpStream::connect(app.base.trim_start_matches("http://"))
        .await
        .unwrap();
    stream
        .write_all(b"POST /a/echo HTTP/1.1\r\nHost: test\r\nContent-Length: 100\r\n\r\nab")
        .await
        .unwrap();
    let mut buf = [0; 1024];
    let start = std::time::Instant::now();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(4), stream.read(&mut buf)).await;
    assert!(
        start.elapsed() >= std::time::Duration::from_millis(900),
        "the backend should have waited for the body until the timeout"
    );

    let (status, body) = app.get("/a/health").await;
    assert_eq!((status, body), (StatusCode::OK, b[0].clone()));
}

#[tokio::test]
async fn wildcard_virtual_hosts_serve_any_subdomain() {
    let b = TestApp::create_backends(2).await.unwrap();
    let ports = Ports::new();
    let routes = route("/a/*", &[&b[0]], "host = \"*.example.com\"", "")
        + &route("/a/*", &[&b[1]], "host = \"www.example.com\"", "");
    let app = ReloadApp::start(config(&ports, "", &routes), None).await;

    let get = |host: &'static str| app.get_with_host("/a/health", Some(host));
    assert_eq!(get("api.example.com").await, (StatusCode::OK, b[0].clone()));
    assert_eq!(
        get("x.y.example.com:3500").await,
        (StatusCode::OK, b[0].clone())
    );
    assert_eq!(
        get("www.example.com").await,
        (StatusCode::OK, b[1].clone()),
        "exact first"
    );
    assert_eq!(
        get("example.com").await.0,
        StatusCode::NOT_FOUND,
        "not the bare domain"
    );
}

/// The value of the sample of `name` with all of `labels`, from Prometheus text output.
fn sample(text: &str, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .filter(|l| l.split(['{', ' ']).next() == Some(name))
        .find(|l| {
            labels
                .iter()
                .all(|(k, v)| l.contains(&format!("{k}=\"{v}\"")))
        })
        .and_then(|l| l.rsplit(' ').next()?.parse().ok())
}

#[tokio::test]
async fn route_and_backend_metrics_are_exported() {
    let b = TestApp::create_backends(1).await.unwrap();
    let down = scripted_backend(Some(UNAVAILABLE)).await;
    let ports = Ports::new();
    let routes = route(
        "/a/*",
        &[&b[0], &down],
        "[proxy.router.retry]\nretry_on_status = [503]",
        "strategy = \"RoundRobin\"",
    ) + &route("/b/*", &[&b[0]], "", "");
    let app = ReloadApp::start(config(&ports, "", &routes), None).await;
    let scrape = || async {
        reqwest::get(format!("http://127.0.0.1:{}/", ports.prometheus))
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    };

    assert_eq!(
        statuses(&app, reqwest::Method::GET, "/a/health", 4).await,
        [200; 4]
    );
    assert_eq!(app.get("/nowhere").await.0, StatusCode::NOT_FOUND);
    let text = scrape().await;

    let requests = |route, status| {
        sample(
            &text,
            "routini_requests_total",
            &[("route", route), ("status", status)],
        )
    };
    assert_eq!(requests("/a/*", "200"), Some(4.0), "{text}");
    // The startup probe (GET /) is unmatched too
    assert!(requests("-", "404").is_some_and(|n| n >= 1.0), "{text}");
    assert_eq!(
        sample(
            &text,
            "routini_request_duration_seconds_count",
            &[("route", "/a/*")]
        ),
        Some(4.0)
    );
    // Round robin over two backends: the 503 one was hit and retried
    let failures = sample(
        &text,
        "routini_upstream_failures_total",
        &[("route", "/a/*"), ("backend", &down), ("kind", "status")],
    );
    assert!(failures.is_some_and(|n| n >= 1.0), "{text}");
    assert!(
        sample(
            &text,
            "routini_upstream_response_seconds_count",
            &[("route", "/a/*"), ("backend", &b[0])]
        )
        .is_some_and(|n| n >= 2.0)
    );
    let backend = [("route", "/a/*"), ("backend", b[0].as_str())];
    assert_eq!(
        sample(&text, "routini_backend_healthy", &backend),
        Some(1.0)
    );
    assert_eq!(
        sample(&text, "routini_backend_active_connections", &backend),
        Some(0.0)
    );
    assert!(sample(&text, "routini_backend_latency_ewma_seconds", &backend).is_some());
    // A plain-HTTP backend: connections took a TCP connect and no TLS handshake
    let connect = |layer| {
        sample(
            &text,
            "routini_upstream_connect_seconds_count",
            &[
                ("route", "/a/*"),
                ("backend", b[0].as_str()),
                ("layer", layer),
            ],
        )
    };
    assert!(connect("tcp").is_some_and(|n| n >= 1.0), "{text}");
    assert_eq!(connect("tls"), None);
    assert_eq!(
        sample(
            &text,
            "routini_route_strategy",
            &[("route", "/a/*"), ("strategy", "RoundRobin")]
        ),
        Some(1.0)
    );

    // A route removed by a reload stops being reported
    app.manager
        .reload(config(&ports, "", &route("/b/*", &[&b[0]], "", "")))
        .await
        .unwrap();
    let text = scrape().await;
    assert!(sample(&text, "routini_backend_healthy", &[("route", "/a/*")]).is_none());
    assert!(sample(&text, "routini_backend_healthy", &[("route", "/b/*")]).is_some());
}

/// An HTTPS backend with a self-signed certificate for `name`. Client certificates are
/// optional unless `require_client_ca` names a CA PEM; the body says whether one was presented
/// (`client=yes` / `client=no`). Returns the address and the certificate PEM.
async fn tls_backend(name: &str, require_client_ca: Option<String>) -> (String, String) {
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::rustls::{
        self, RootCertStore,
        pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
        server::WebPkiClientVerifier,
    };
    let cert = rcgen::generate_simple_self_signed(vec![name.to_string()]).unwrap();
    let chain = vec![CertificateDer::from(cert.cert.der().to_vec())];
    let key = PrivateKeyDer::try_from(cert.key_pair.serialize_der()).unwrap();
    let builder = rustls::ServerConfig::builder();
    let config = match require_client_ca {
        Some(ca_pem) => {
            let mut roots = RootCertStore::empty();
            for ca in CertificateDer::pem_slice_iter(ca_pem.as_bytes()) {
                roots.add(ca.unwrap()).unwrap();
            }
            let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
                .allow_unauthenticated()
                .build()
                .unwrap();
            builder.with_client_cert_verifier(verifier)
        }
        None => builder.with_no_client_auth(),
    }
    .with_single_cert(chain, key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(stream).await else {
                    return;
                };
                let client = tls
                    .get_ref()
                    .1
                    .peer_certificates()
                    .is_some_and(|c| !c.is_empty());
                let body = format!("client={}", if client { "yes" } else { "no" });
                // Keep-alive, so the proxy pools the connection
                let mut buf = [0; 4096];
                let mut request = Vec::new();
                loop {
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match tls.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    request.clear();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    if tls.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (address, cert.cert.pem())
}

#[tokio::test]
async fn upstream_certificate_verification_can_be_turned_off() {
    let (backend, _) = tls_backend("backend.internal", None).await;
    let ports = Ports::new();
    let routes = route(
        "/a/*",
        &[&backend],
        "[proxy.router.upstream_tls]\nenabled = true\nsni = \"backend.internal\"\nverify = false",
        "",
    );
    let app = ReloadApp::start(config(&ports, "", &routes), None).await;
    assert_eq!(
        app.get("/a/x").await,
        (StatusCode::OK, "client=no".to_string())
    );
    let text = reqwest::get(format!("http://127.0.0.1:{}/", ports.prometheus))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let handshakes = sample(
        &text,
        "routini_upstream_connect_seconds_count",
        &[("route", "/a/*"), ("backend", &backend), ("layer", "tls")],
    );
    assert!(handshakes.is_some_and(|n| n >= 1.0), "{text}");
}

#[tokio::test]
async fn backends_can_be_verified_against_a_private_ca() {
    let (backend, backend_cert) = tls_backend("backend.internal", None).await;
    let dir = std::env::temp_dir().join(format!("routini-upstream-ca-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ca = dir.join("ca.pem");
    std::fs::write(&ca, &backend_cert).unwrap();

    let ports = Ports::new();
    let tls = "[proxy.router.upstream_tls]\nenabled = true\nsni = \"backend.internal\"";
    let routes = route(
        "/a/*",
        &[&backend],
        &format!("{tls}\nca_path = \"{}\"", ca.display()),
        "",
    ) + &route("/system/*", &[&backend], tls, "");
    let config_for = || config(&ports, "", &routes);
    let app = ReloadApp::start(config_for(), None).await;

    assert_eq!(
        app.get("/a/x").await,
        (StatusCode::OK, "client=no".to_string())
    );
    // Same backend and SNI: the connection /a/* just pooled was verified against its CA only
    // and must not be handed to a route trusting the system CAs
    assert_eq!(
        app.get("/system/x").await.0,
        StatusCode::BAD_GATEWAY,
        "not a public CA"
    );

    // A CA file replaced on disk is picked up by a reload of the same config
    let report = app.manager.reload(config_for()).await.unwrap();
    assert!(!report.updated.contains(&"/a/*".to_string()), "{report:?}");
    let other = rcgen::generate_simple_self_signed(vec!["other".to_string()]).unwrap();
    std::fs::write(&ca, other.cert.pem()).unwrap();
    let report = app.manager.reload(config_for()).await.unwrap();
    assert!(report.updated.contains(&"/a/*".to_string()), "{report:?}");
    assert_eq!(
        app.get("/a/x").await.0,
        StatusCode::BAD_GATEWAY,
        "now the wrong CA"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_client_certificate_is_presented_to_backends() {
    use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair};
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(vec![]).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let client_key = KeyPair::generate().unwrap();
    let mut client_params = CertificateParams::new(vec!["routini".to_string()]).unwrap();
    client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let client = client_params.signed_by(&client_key, &ca, &ca_key).unwrap();

    let dir = std::env::temp_dir().join(format!("routini-upstream-mtls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("client.pem"), client.pem()).unwrap();
    std::fs::write(dir.join("client.key"), client_key.serialize_pem()).unwrap();

    let (backend, _) = tls_backend("backend.internal", Some(ca.pem())).await;
    let ports = Ports::new();
    let tls =
        "[proxy.router.upstream_tls]\nenabled = true\nsni = \"backend.internal\"\nverify = false";
    let routes = route(
        "/a/*",
        &[&backend],
        &format!(
            "{tls}\nclient_cert_path = \"{}\"\nclient_key_path = \"{}\"",
            dir.join("client.pem").display(),
            dir.join("client.key").display()
        ),
        "",
    ) + &route("/anonymous/*", &[&backend], tls, "");
    let app = ReloadApp::start(config(&ports, "", &routes), None).await;

    assert_eq!(
        app.get("/a/x").await,
        (StatusCode::OK, "client=yes".to_string())
    );
    assert_eq!(
        app.get("/anonymous/x").await,
        (StatusCode::OK, "client=no".to_string())
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `GET path` on a fresh connection, closed after the response: the status line, or `None` if
/// the proxy closed the connection without answering.
async fn raw_get(port: u16, path: &str) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .ok()?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut response = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_to_end(&mut response),
    )
    .await
    .ok()?
    .ok()?;
    let response = String::from_utf8_lossy(&response);
    response.lines().next().map(str::to_string)
}

#[tokio::test]
async fn connections_over_the_listener_limit_are_closed() {
    let b = TestApp::create_backends(1).await.unwrap();
    let ports = Ports::new();
    let routes = route("/a/*", &[&b[0]], "", "");
    let app = ReloadApp::start(
        config(
            &ports,
            "keepalive_timeout_secs = 0\nlistener_limits = { max_connections = 1 }",
            &routes,
        ),
        None,
    )
    .await;
    let metric = |name: &'static str, labels: &'static [(&'static str, &'static str)]| async move {
        let text = reqwest::get(format!("http://127.0.0.1:{}/", ports.prometheus))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        sample(&text, name, labels)
    };
    let open = || metric("routini_listener_open_connections", &[("listener", "http")]);

    // An idle client takes the only slot, once the startup probe's connection is gone
    let idle = tokio::net::TcpStream::connect(("127.0.0.1", ports.listen))
        .await
        .unwrap();
    let mut counted = false;
    for _ in 0..50 {
        if open().await == Some(1.0) {
            counted = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(counted, "open connections: {:?}", open().await);

    assert_eq!(raw_get(ports.listen, "/a/health").await, None);
    let closed = metric(
        "routini_listener_closed_total",
        &[("listener", "http"), ("reason", "connection_limit")],
    )
    .await;
    assert!(closed.is_some_and(|n| n >= 1.0), "{closed:?}");

    // Its slot frees up when it leaves
    drop(idle);
    let mut served = None;
    for _ in 0..50 {
        served = raw_get(ports.listen, "/a/health").await;
        if served.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(served.as_deref(), Some("HTTP/1.1 200 OK"));
    drop(app);
}
