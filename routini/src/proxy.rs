use http::StatusCode;
use matchit::Router;
use std::sync::Arc;

use pingora::{
    Error, ErrorSource, ErrorType, ImmutStr, Result, RetryType,
    prelude::HttpPeer,
    proxy::{ProxyHttp, Session},
    upstreams::peer::Tracer,
};

use crate::{
    load_balancing::{
        LoadBalancer,
        strategy::{Adaptive, fewest_connections::ConnectionsTracer},
    },
    server_builder::RouteConfig,
};

type MaxIterations = usize;

pub struct RouteValue {
    pub lb: Arc<LoadBalancer<Adaptive>>,
    pub max_iterations: MaxIterations,
    pub route_config: RouteConfig,
}

#[derive(Clone)]
pub struct Proxy {
    routes: Arc<Router<RouteValue>>,
}

impl Proxy {
    pub fn new(routes: Router<RouteValue>) -> Self {
        Proxy {
            routes: Arc::new(routes),
        }
    }

    pub fn route(&self, path: &str) -> Result<(&RouteValue, Option<String>)> {
        tracing::info!("route path: {}", path);
        self.routes
            .at(path)
            .map_err(|e| {
                Box::new(Error {
                    cause: Some(Box::new(e)),
                    context: Some(ImmutStr::Static("Failed to route path to backend")),
                    esource: ErrorSource::Internal,
                    etype: ErrorType::HTTPStatus(StatusCode::BAD_REQUEST.as_u16()),
                    retry: RetryType::Decided(false),
                })
            })
            .map(|m| {
                let value = m.value;
                let stripped_path = if value.route_config.strip_path_prefix {
                    m.params.get("rest").map(|p| {
                        if !p.starts_with('/') {
                            format!("/{p}")
                        } else {
                            p.to_string()
                        }
                    })
                } else {
                    None
                };
                (value, stripped_path)
            })
    }
}

#[async_trait::async_trait]
impl ProxyHttp for Proxy {
    type CTX = Option<String>;

    fn new_ctx(&self) -> Self::CTX {
        None
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let path = session.req_header().uri.path();
        let (route_value, new_path) = self.route(path)?;

        let backend = route_value
            .lb
            .select(&[], route_value.max_iterations)
            .ok_or(Error {
                context: Some(ImmutStr::Static("No healthy backends available")),
                cause: None,
                etype: ErrorType::InternalError,
                esource: ErrorSource::Internal,
                retry: RetryType::Decided(true),
            })?;

        if let Some(path) = new_path {
            session.req_header_mut().set_raw_path(path.as_bytes())?;
        }

        let tracer = Tracer(Box::new(ConnectionsTracer(backend.addr.clone())));
        let mut peer = Box::new(HttpPeer::new(backend, false, String::new()));
        peer.options.tracer = Some(tracer);
        Ok(peer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load_balancing::strategy::Adaptive;

    fn create_test_route_value(strip_path: bool) -> RouteValue {
        let backends = vec!["127.0.0.1:8080".to_string()];
        let lb = LoadBalancer::try_from_iter_with_strategy(backends, Adaptive::default())
            .expect("Failed to create load balancer");

        RouteValue {
            lb: Arc::new(lb),
            max_iterations: 10,
            route_config: RouteConfig {
                strip_path_prefix: strip_path,
            },
        }
    }

    #[test]
    fn test_route_basic_matching() {
        let mut router = Router::new();
        router
            .insert("/api", create_test_route_value(false))
            .expect("Failed to insert route");

        let proxy = Proxy::new(router);
        let result = proxy.route("/api");

        assert!(result.is_ok());
        let (route_value, stripped_path) = result.unwrap();
        assert_eq!(route_value.max_iterations, 10);
        assert_eq!(stripped_path, None);
    }

    #[test]
    fn test_route_with_wildcard() {
        let mut router = Router::new();
        router
            .insert("/api/{*rest}", create_test_route_value(false))
            .expect("Failed to insert route");

        let proxy = Proxy::new(router);
        let result = proxy.route("/api/users/123");

        assert!(result.is_ok());
        let (route_value, stripped_path) = result.unwrap();
        assert_eq!(route_value.max_iterations, 10);
        assert_eq!(stripped_path, None);
    }

    #[test]
    fn test_route_with_path_stripping() {
        let mut router = Router::new();
        router
            .insert("/auth/{*rest}", create_test_route_value(true))
            .expect("Failed to insert route");

        let proxy = Proxy::new(router);
        let result = proxy.route("/auth/login");

        assert!(result.is_ok());
        let (route_value, stripped_path) = result.unwrap();
        assert!(route_value.route_config.strip_path_prefix);
        assert_eq!(stripped_path, Some("login".to_string()));
    }

    #[test]
    fn test_route_with_path_stripping_nested() {
        let mut router = Router::new();
        router
            .insert("/api/v1/{*rest}", create_test_route_value(true))
            .expect("Failed to insert route");

        let proxy = Proxy::new(router);
        let result = proxy.route("/api/v1/users/123/profile");

        assert!(result.is_ok());
        let (route_value, stripped_path) = result.unwrap();
        assert!(route_value.route_config.strip_path_prefix);
        assert_eq!(stripped_path, Some("users/123/profile".to_string()));
    }

    #[test]
    fn test_route_not_found() {
        let mut router = Router::new();
        router
            .insert("/api", create_test_route_value(false))
            .expect("Failed to insert route");

        let proxy = Proxy::new(router);
        let result = proxy.route("/nonexistent");

        assert!(result.is_err());
        if let Err(err) = result {
            assert_eq!(
                err.etype,
                ErrorType::HTTPStatus(StatusCode::BAD_REQUEST.as_u16())
            );
        }
    }

    #[test]
    fn test_route_multiple_routes() {
        let mut router = Router::new();
        router
            .insert("/api", create_test_route_value(false))
            .expect("Failed to insert route");
        router
            .insert("/auth/{*rest}", create_test_route_value(true))
            .expect("Failed to insert route");

        let proxy = Proxy::new(router);

        let result = proxy.route("/api");
        assert!(result.is_ok());
        let (_, stripped) = result.unwrap();
        assert_eq!(stripped, None);

        let result = proxy.route("/auth/logout");
        assert!(result.is_ok());
        let (route_value, stripped) = result.unwrap();
        assert!(route_value.route_config.strip_path_prefix);
        assert_eq!(stripped, Some("logout".to_string()));
    }

    #[test]
    fn test_route_exact_vs_wildcard_priority() {
        let mut router = Router::new();
        let mut exact_route = create_test_route_value(false);
        exact_route.max_iterations = 5;

        router
            .insert("/api/health", exact_route)
            .expect("Failed to insert exact route");
        router
            .insert("/api/{*rest}", create_test_route_value(false))
            .expect("Failed to insert wildcard route");

        let proxy = Proxy::new(router);

        let result = proxy.route("/api/health");
        assert!(result.is_ok());
        let (route_value, _) = result.unwrap();
        assert_eq!(route_value.max_iterations, 5);

        let result = proxy.route("/api/users");
        assert!(result.is_ok());
        let (route_value, _) = result.unwrap();
        assert_eq!(route_value.max_iterations, 10);
    }

    #[test]
    fn test_route_empty_path() {
        let mut router = Router::new();
        router
            .insert("/", create_test_route_value(false))
            .expect("Failed to insert route");

        let proxy = Proxy::new(router);
        let result = proxy.route("/");

        assert!(result.is_ok());
    }

    #[test]
    fn test_route_with_query_params() {
        let mut router = Router::new();
        router
            .insert("/search/{*rest}", create_test_route_value(true))
            .expect("Failed to insert route");

        let proxy = Proxy::new(router);
        let result = proxy.route("/search/query?q=test");

        assert!(result.is_ok());
        let (_, stripped_path) = result.unwrap();
        assert_eq!(stripped_path, Some("query?q=test".to_string()));
    }
}
