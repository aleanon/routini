use std::fmt::Display;

use http::{Method, Response, StatusCode};
use pingora::{
    apps::http_app::ServeHttp, protocols::http::ServerSession, services::listening::Service,
};
use serde::Deserialize;
use tracing::{error, info};

use crate::{
    load_balancing::strategy::Adaptive, proxy::Proxy, utils::constants::SET_STRATEGY_ENDPOINT_NAME,
};

impl Display for Adaptive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Adaptive::RoundRobin => write!(f, "RoundRobin"),
            Adaptive::Random => write!(f, "Random"),
            Adaptive::FNVHash => write!(f, "FNVHash"),
            Adaptive::FewestConnections => write!(f, "FewestConnection"),
            Adaptive::Consistent => write!(f, "Consistent"),
        }
    }
}

#[derive(Deserialize)]
struct NewStrategy {
    path: String,
    strategy: Adaptive,
}

/// Temporary endpoint for updating the load balancer strategy,
/// This should be automatically decided by an internal task
pub struct SetStrategyEndpoint {
    pub router: Proxy,
}

impl SetStrategyEndpoint {
    pub fn service(router: Proxy, address: &str) -> Service<Self> {
        let mut service = Service::new(SET_STRATEGY_ENDPOINT_NAME.to_string(), Self { router });
        service.add_tcp(address);
        service
    }
}

#[async_trait::async_trait]
impl ServeHttp for SetStrategyEndpoint {
    async fn response(&self, http_session: &mut ServerSession) -> Response<Vec<u8>> {
        match http_session {
            ServerSession::H1(session) => {
                let request_header = session.req_header();
                if request_header.method != Method::POST {
                    return response(StatusCode::METHOD_NOT_ALLOWED);
                }

                let body = match session.read_body_bytes().await {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        error!("Failed to read body bytes: {}", err);
                        return response(StatusCode::INTERNAL_SERVER_ERROR);
                    }
                };
                let Some(body) = body else {
                    error!("Empty body");
                    return response(StatusCode::INTERNAL_SERVER_ERROR);
                };

                match serde_json::from_slice::<NewStrategy>(&body) {
                    Ok(NewStrategy { path, strategy }) => match self.router.route(&path) {
                        Ok((route_value, _)) => {
                            route_value.lb.update_strategy(strategy.clone());
                            info!("Strategy updated to {}", strategy);
                            response(StatusCode::OK)
                        }
                        Err(err) => {
                            error!("{err}");
                            response(StatusCode::BAD_REQUEST)
                        }
                    },
                    Err(_) => response(StatusCode::BAD_REQUEST),
                }
            }
            ServerSession::H2(_) => response(StatusCode::BAD_REQUEST),
        }
    }
}

fn response(status: StatusCode) -> Response<Vec<u8>> {
    let mut response = Response::new(Vec::new());
    *response.status_mut() = status;
    response
}
