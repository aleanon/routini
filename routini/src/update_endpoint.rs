use std::sync::Arc;

use http::{Method, Response, StatusCode};
use pingora::{
    apps::http_app::ServeHttp, protocols::http::ServerSession, services::listening::Service,
};
use serde::de::DeserializeOwned;
use tracing::{error, info};

use crate::load_balancing::{
    LoadBalancer,
    selection::{BackendIter, BackendSelection, Strategy},
};

/// Temporary endpoint for updating the load balancer strategy,
/// This should be automatically decided by an internal task
#[derive(Clone)]
pub struct UpdateStrategyEndpoint<S>
where
    S: Strategy + Send + Sync + 'static,
    S::Selector: BackendSelection + Send + Sync,
    <S::Selector as BackendSelection>::Iter: BackendIter,
{
    pub load_balancer: Arc<LoadBalancer<S>>,
}

impl<S> UpdateStrategyEndpoint<S>
where
    S: Strategy + Send + Sync + 'static + DeserializeOwned,
    S::Selector: BackendSelection + Send + Sync,
    <S::Selector as BackendSelection>::Iter: BackendIter,
{
    pub fn service(load_balancer: Arc<LoadBalancer<S>>, address: &str) -> Service<Self> {
        let mut service = Service::new("strategy_updater".to_string(), Self { load_balancer });
        service.add_tcp(address);
        service
    }
}

#[async_trait::async_trait]
impl<S> ServeHttp for UpdateStrategyEndpoint<S>
where
    S: Strategy + Send + Sync + 'static + DeserializeOwned,
    S::Selector: BackendSelection + Send + Sync,
    <S::Selector as BackendSelection>::Iter: BackendIter,
{
    async fn response(&self, http_session: &mut ServerSession) -> Response<Vec<u8>> {
        info!("request received");
        match http_session {
            ServerSession::H1(session) => {
                let request_header = session.req_header();
                if request_header.method != Method::POST {
                    return Response::builder()
                        .status(StatusCode::METHOD_NOT_ALLOWED)
                        .body(Vec::new())
                        .unwrap();
                }
                let body = match session.read_body_bytes().await {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        error!("Failed to read body bytes: {}", err);
                        return Response::builder()
                            .status(StatusCode::INTERNAL_SERVER_ERROR)
                            .body(Vec::new())
                            .unwrap();
                    }
                };
                let Some(body) = body else {
                    error!("Failed to read body bytes");
                    return Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Vec::new())
                        .unwrap();
                };

                match serde_json::from_slice::<S>(&body) {
                    Ok(strategy) => self.load_balancer.update_strategy(strategy),
                    Err(_) => {
                        return Response::builder()
                            .status(StatusCode::BAD_REQUEST)
                            .body(Vec::new())
                            .unwrap();
                    }
                };
                info!("Strategy updated");
                Response::builder()
                    .status(StatusCode::OK)
                    .body(Vec::new())
                    .expect("Failed to build H1 response for strategy update endpoint")
            }
            ServerSession::H2(_) => Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Vec::new())
                .expect("Failed to build H2 response for strategy update endpoint"),
        }
    }
}
