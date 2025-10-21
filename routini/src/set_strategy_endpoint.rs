use std::sync::Arc;

use http::{Method, Response, StatusCode};
use pingora::{
    apps::http_app::ServeHttp, protocols::http::ServerSession, services::listening::Service,
};
use serde::de::DeserializeOwned;
use tracing::{error, info};

use crate::{
    load_balancing::{LoadBalancer, strategy::Strategy},
    utils::constants::SET_STRATEGY_ENDPOINT_NAME,
};

/// Temporary endpoint for updating the load balancer strategy,
/// This should be automatically decided by an internal task
#[derive(Clone)]
pub struct SetStrategyEndpoint<S>
where
    S: Strategy + 'static,
{
    pub load_balancer: Arc<LoadBalancer<S>>,
}

impl<S> SetStrategyEndpoint<S>
where
    S: Strategy + 'static + DeserializeOwned,
{
    pub fn service(load_balancer: Arc<LoadBalancer<S>>, address: &str) -> Service<Self> {
        let mut service = Service::new(
            SET_STRATEGY_ENDPOINT_NAME.to_string(),
            Self { load_balancer },
        );
        service.add_tcp(address);
        service
    }
}

#[async_trait::async_trait]
impl<S> ServeHttp for SetStrategyEndpoint<S>
where
    S: Strategy + 'static + DeserializeOwned,
{
    async fn response(&self, http_session: &mut ServerSession) -> Response<Vec<u8>> {
        info!("request received");
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
                    error!("Failed to read body bytes");
                    return response(StatusCode::INTERNAL_SERVER_ERROR);
                };

                match serde_json::from_slice::<S>(&body) {
                    Ok(strategy) => {
                        self.load_balancer.update_strategy(strategy);
                        info!("Strategy updated");
                        response(StatusCode::OK)
                    }
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
