use std::sync::Arc;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use http_body_util::BodyExt;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Method};
use hyper::body::Incoming;
use tracing::{info, error, warn, debug};
use anyhow::Result;
use dashmap::DashMap;
use parking_lot::RwLock;

use crate::config::ProxyConfig;
use crate::ai::AiAgent;
use super::load_balancer::LoadBalancer;
use super::circuit_breaker::CircuitBreaker;
use super::retry::RetryPolicy;
use super::middleware::{Middleware, MiddlewareChain};

pub struct ProxyServer {
    config: Arc<ProxyConfig>,
    load_balancer: Arc<LoadBalancer>,
    circuit_breaker: Arc<CircuitBreaker>,
    retry_policy: Arc<RetryPolicy>,
    middleware_chain: Arc<MiddlewareChain>,
    ai_agent: Option<Arc<AiAgent>>,
    metrics: Arc<ProxyMetrics>,
    active_connections: Arc<DashMap<SocketAddr, ConnectionInfo>>,
}

#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    pub established_at: std::time::Instant,
    pub requests_count: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

#[derive(Debug, Default)]
pub struct ProxyMetrics {
    pub total_requests: parking_lot::RwLock<u64>,
    pub successful_requests: parking_lot::RwLock<u64>,
    pub failed_requests: parking_lot::RwLock<u64>,
    pub active_connections: parking_lot::RwLock<u64>,
    pub response_times: parking_lot::RwLock<Vec<u64>>,
    pub upstream_health: DashMap<String, bool>,
}

impl ProxyServer {
    pub async fn new(config: ProxyConfig, ai_enabled: bool) -> Result<Self> {
        let config = Arc::new(config);
        
        let load_balancer = Arc::new(LoadBalancer::new(config.clone()).await?);
        let circuit_breaker = Arc::new(CircuitBreaker::new(config.clone()));
        let retry_policy = Arc::new(RetryPolicy::new(config.clone()));
        let middleware_chain = Arc::new(MiddlewareChain::new(config.clone()).await?);
        
        let ai_agent = if ai_enabled && config.ai.enabled {
            Some(Arc::new(AiAgent::new(config.clone()).await?))
        } else {
            None
        };
        
        let metrics = Arc::new(ProxyMetrics::default());
        let active_connections = Arc::new(DashMap::new());
        
        Ok(Self {
            config,
            load_balancer,
            circuit_breaker,
            retry_policy,
            middleware_chain,
            ai_agent,
            metrics,
            active_connections,
        })
    }
    
    pub async fn run(&mut self) -> Result<()> {
        let addr = format!("{}:{}", self.config.server.bind_address, self.config.server.port);
        let listener = TcpListener::bind(&addr).await?;
        
        info!("Sidecar proxy listening on {}", addr);
        
        self.start_health_checks().await?;
        self.start_metrics_server().await?;
        
        if let Some(ai_agent) = &self.ai_agent {
            self.start_ai_monitoring(ai_agent.clone()).await?;
        }
        
        loop {
            let (stream, remote_addr) = listener.accept().await?;
            
            self.active_connections.insert(remote_addr, ConnectionInfo {
                established_at: std::time::Instant::now(),
                requests_count: 0,
                bytes_sent: 0,
                bytes_received: 0,
            });
            
            let server = self.clone();
            let connections = self.active_connections.clone();
            tokio::spawn(async move {
                let server_clone = server.clone();
                if let Err(err) = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service_fn(move |req| {
                        let server = server_clone.clone();
                        async move {
                            server.handle_request(req, remote_addr).await
                        }
                    }))
                    .await
                {
                    error!("Connection error: {}", err);
                }
                
                connections.remove(&remote_addr);
            });
        }
    }
    
    async fn handle_request(
        &self,
        mut req: Request<Incoming>,
        client_addr: SocketAddr,
    ) -> Result<Response<String>, hyper::Error> {
        let start_time = std::time::Instant::now();
        
        {
            let mut total = self.metrics.total_requests.write();
            *total += 1;
        }
        
        if let Err(response) = self.middleware_chain.process_request(&mut req, client_addr).await {
            return Ok(response);
        }
        
        let service_name = self.extract_service_name(&req);
        
        if !self.circuit_breaker.can_execute(&service_name).await {
            warn!("Circuit breaker open for service: {}", service_name);
            return Ok(Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body("Service temporarily unavailable".to_string())
                .unwrap());
        }
        
        let endpoint = match self.load_balancer.select_endpoint(&service_name).await {
            Some(endpoint) => endpoint,
            None => {
                error!("No healthy endpoints available for service: {}", service_name);
                return Ok(Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body("No healthy upstream servers".to_string())
                    .unwrap());
            }
        };
        
        let uri_path = req.uri().path().to_string();
        let method = req.method().clone();
        
        let response = self.forward_request(req, &endpoint).await;
        
        let duration = start_time.elapsed();
        
        match response {
            Ok(resp) => {
                {
                    let mut successful = self.metrics.successful_requests.write();
                    *successful += 1;
                }
                
                self.circuit_breaker.record_success(&service_name).await;
                
                
                Ok(resp)
            }
            Err(err) => {
                error!("Request failed: {}", err);
                
                {
                    let mut failed = self.metrics.failed_requests.write();
                    *failed += 1;
                }
                
                self.circuit_breaker.record_failure(&service_name).await;
                
                Ok(Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body("Internal server error".to_string())
                    .unwrap())
            }
        }
    }
    
    async fn forward_request(
        &self,
        mut req: Request<Incoming>,
        endpoint: &str,
    ) -> Result<Response<String>> {
        let uri = format!("http://{}{}", endpoint, req.uri().path_and_query().map(|x| x.as_str()).unwrap_or("/"));
        *req.uri_mut() = uri.parse()?;
        
        req.headers_mut().insert("X-Forwarded-Proto", "http".parse()?);
        req.headers_mut().insert("X-Forwarded-For", "127.0.0.1".parse()?);
        
        let client = reqwest::Client::new();
        let method = match req.method() {
            &Method::GET => reqwest::Method::GET,
            &Method::POST => reqwest::Method::POST,
            &Method::PUT => reqwest::Method::PUT,
            &Method::DELETE => reqwest::Method::DELETE,
            &Method::PATCH => reqwest::Method::PATCH,
            &Method::HEAD => reqwest::Method::HEAD,
            &Method::OPTIONS => reqwest::Method::OPTIONS,
            _ => reqwest::Method::GET,
        };
        
        let body_bytes = req.into_body().collect().await?.to_bytes();
        
        let response = client
            .request(method, &uri)
            .body(body_bytes.to_vec())
            .send()
            .await?;
        
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.bytes().await?;
        
        let mut builder = Response::builder().status(status.as_u16());
        
        for (key, value) in headers.iter() {
            builder = builder.header(key.as_str(), value.as_bytes());
        }
        
        Ok(builder.body(String::from_utf8_lossy(&body).to_string())?)
    }
    
    fn extract_service_name(&self, req: &Request<Incoming>) -> String {
        req.headers()
            .get("x-service-name")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("default")
            .to_string()
    }
    
    async fn start_health_checks(&self) -> Result<()> {
        let load_balancer = self.load_balancer.clone();
        tokio::spawn(async move {
            load_balancer.start_health_checks().await;
        });
        Ok(())
    }
    
    async fn start_metrics_server(&self) -> Result<()> {
        if !self.config.observability.metrics.enabled {
            return Ok(());
        }
        
        let metrics = self.metrics.clone();
        let port = self.config.observability.metrics.prometheus_port;
        
        tokio::spawn(async move {
            let addr = format!("0.0.0.0:{}", port);
            let listener = TcpListener::bind(&addr).await.unwrap();
            info!("Metrics server listening on {}", addr);
            
            loop {
                if let Ok((stream, _)) = listener.accept().await {
                    let metrics = metrics.clone();
                    tokio::spawn(async move {
                        let service = service_fn(move |_req| {
                            let metrics = metrics.clone();
                            async move {
                                let total_requests = *metrics.total_requests.read();
                                let successful_requests = *metrics.successful_requests.read();
                                let failed_requests = *metrics.failed_requests.read();
                                
                                let prometheus_metrics = format!(
                                    "# HELP proxy_requests_total Total number of requests\n\
                                     # TYPE proxy_requests_total counter\n\
                                     proxy_requests_total {}\n\
                                     # HELP proxy_requests_successful_total Total number of successful requests\n\
                                     # TYPE proxy_requests_successful_total counter\n\
                                     proxy_requests_successful_total {}\n\
                                     # HELP proxy_requests_failed_total Total number of failed requests\n\
                                     # TYPE proxy_requests_failed_total counter\n\
                                     proxy_requests_failed_total {}\n",
                                    total_requests, successful_requests, failed_requests
                                );
                                
                                Ok::<_, hyper::Error>(Response::new(prometheus_metrics))
                            }
                        });
                        
                        if let Err(err) = http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await
                        {
                            error!("Metrics server error: {}", err);
                        }
                    });
                }
            }
        });
        
        Ok(())
    }
    
    async fn start_ai_monitoring(&self, ai_agent: Arc<AiAgent>) -> Result<()> {
        let metrics = self.metrics.clone();
        let config = self.config.clone();
        
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            
            loop {
                interval.tick().await;
                
                if config.ai.autonomous_mode {
                    ai_agent.analyze_system_health(&metrics).await;
                }
            }
        });
        
        Ok(())
    }
}

impl Clone for ProxyServer {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            load_balancer: self.load_balancer.clone(),
            circuit_breaker: self.circuit_breaker.clone(),
            retry_policy: self.retry_policy.clone(),
            middleware_chain: self.middleware_chain.clone(),
            ai_agent: self.ai_agent.clone(),
            metrics: self.metrics.clone(),
            active_connections: self.active_connections.clone(),
        }
    }
}
