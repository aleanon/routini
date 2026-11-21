//! A real server built from a TOML config, for end-to-end tests of config-driven features.
#![allow(dead_code)]
use std::{collections::HashSet, sync::Arc, time::Duration};

use reqwest::StatusCode;
use routini::{config::Config, reload::RouteManager, utils::config_loader::ConfigFormat};

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A route on `path` with `backends`, health checks off. `route_extra` goes into the
/// `[[proxy.router]]` table, `lb_extra` into its `load_balancer` table.
pub fn route(path: &str, backends: &[&String], route_extra: &str, lb_extra: &str) -> String {
    let upstreams: String = backends
        .iter()
        .map(|b| format!("[[proxy.router.load_balancer.upstreams]]\naddress = \"{b}\"\n"))
        .collect();
    format!(
        "[[proxy.router]]\npath = \"{path}\"\n{route_extra}\n\
         [proxy.router.load_balancer]\nhealth_check_interval_secs = 0\n{lb_extra}\n{upstreams}\n"
    )
}

pub struct Ports {
    pub listen: u16,
    pub prometheus: u16,
}

impl Ports {
    pub fn new() -> Self {
        Self {
            listen: free_port(),
            prometheus: free_port(),
        }
    }

    pub fn toml(&self, routes: &str) -> String {
        format!(
            "[server]\nprometheus_address = \"127.0.0.1:{}\"\ngrace_period_seconds = 0\n\n\
             [proxy]\nlistener = \"127.0.0.1:{}\"\n\n{routes}",
            self.prometheus, self.listen
        )
    }

    pub fn config(&self, routes: &str) -> Config {
        let config = Config::parse(&self.toml(routes), ConfigFormat::Toml).unwrap();
        config.validate().unwrap();
        config
    }
}

pub struct ReloadApp {
    pub base: String,
    pub manager: Arc<RouteManager>,
    pub client: reqwest::Client,
}

impl ReloadApp {
    pub async fn start(config: Config, config_file: Option<String>) -> Self {
        let base = format!("http://{}", config.listen_address());
        let mut builder = config.into_server_builder().unwrap();
        if let Some(path) = config_file {
            builder = builder.reload_on_sighup(path);
        }
        let (server, manager) = builder.build_with_reload_handle();
        std::thread::spawn(move || server.run_forever());

        let app = Self {
            base,
            manager,
            client: reqwest::Client::new(),
        };
        for _ in 0..100 {
            if app.manager.is_running().await && app.client.get(&app.base).send().await.is_ok() {
                return app;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("server did not start");
    }

    /// Status and body (the worker echoes its own address on /health).
    pub async fn get(&self, path: &str) -> (StatusCode, String) {
        self.get_with_host(path, None).await
    }

    pub async fn get_with_host(&self, path: &str, host: Option<&str>) -> (StatusCode, String) {
        let mut req = self.client.get(format!("{}{path}", self.base));
        if let Some(host) = host {
            req = req.header("host", host);
        }
        let res = req.send().await.unwrap();
        (res.status(), res.text().await.unwrap())
    }

    pub async fn served_by(&self, path: &str, requests: usize) -> HashSet<String> {
        let mut seen = HashSet::new();
        for _ in 0..requests {
            let (status, body) = self.get(path).await;
            assert_eq!(status, StatusCode::OK, "{path}: {body}");
            seen.insert(body);
        }
        seen
    }
}
