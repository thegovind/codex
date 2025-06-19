use std::sync::Arc;
use std::net::SocketAddr;
use hyper::{Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::{debug, warn};
use anyhow::Result;
use async_trait::async_trait;
use dashmap::DashMap;
use parking_lot::RwLock;
use std::time::{Duration, Instant};

use crate::config::ProxyConfig;

#[async_trait]
pub trait Middleware: Send + Sync {
    async fn process_request(
        &self,
        req: &mut Request<Incoming>,
        client_addr: SocketAddr,
    ) -> Result<(), Response<String>>;
    
    async fn process_response(
        &self,
        resp: &mut Response<String>,
        client_addr: SocketAddr,
    ) -> Result<(), Response<String>>;
}

pub struct MiddlewareChain {
    middlewares: Vec<Box<dyn Middleware>>,
}

impl MiddlewareChain {
    pub async fn new(config: Arc<ProxyConfig>) -> Result<Self> {
        let mut middlewares: Vec<Box<dyn Middleware>> = Vec::new();
        
        if config.security.rate_limiting.enabled {
            middlewares.push(Box::new(RateLimitingMiddleware::new(config.clone())));
        }
        
        if config.security.authentication.enabled {
            middlewares.push(Box::new(AuthenticationMiddleware::new(config.clone())));
        }
        
        if config.security.authorization.enabled {
            middlewares.push(Box::new(AuthorizationMiddleware::new(config.clone())));
        }
        
        middlewares.push(Box::new(LoggingMiddleware::new(config.clone())));
        
        if config.observability.metrics.enabled {
            middlewares.push(Box::new(MetricsMiddleware::new(config.clone())));
        }
        
        middlewares.push(Box::new(CorsMiddleware::new(config.clone())));
        
        Ok(Self { middlewares })
    }
    
    pub async fn process_request(
        &self,
        req: &mut Request<Incoming>,
        client_addr: SocketAddr,
    ) -> Result<(), Response<String>> {
        for middleware in &self.middlewares {
            middleware.process_request(req, client_addr).await?;
        }
        Ok(())
    }
    
    pub async fn process_response(
        &self,
        resp: &mut Response<String>,
        client_addr: SocketAddr,
    ) -> Result<(), Response<String>> {
        for middleware in self.middlewares.iter().rev() {
            middleware.process_response(resp, client_addr).await?;
        }
        Ok(())
    }
}

pub struct RateLimitingMiddleware {
    config: Arc<ProxyConfig>,
    buckets: DashMap<String, TokenBucket>,
}

#[derive(Debug)]
struct TokenBucket {
    tokens: RwLock<f64>,
    last_refill: RwLock<Instant>,
    capacity: f64,
    refill_rate: f64,
}

impl RateLimitingMiddleware {
    pub fn new(config: Arc<ProxyConfig>) -> Self {
        Self {
            config,
            buckets: DashMap::new(),
        }
    }
    
    fn extract_key(&self, req: &Request<Incoming>, client_addr: SocketAddr) -> String {
        match self.config.security.rate_limiting.key_extractor.as_str() {
            "ip" => client_addr.ip().to_string(),
            "header" => {
                req.headers()
                    .get("x-rate-limit-key")
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or(&client_addr.ip().to_string())
                    .to_string()
            }
            "jwt_claim" => {
                req.headers()
                    .get("authorization")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|auth| auth.strip_prefix("Bearer "))
                    .unwrap_or(&client_addr.ip().to_string())
                    .to_string()
            }
            _ => client_addr.ip().to_string(),
        }
    }
    
    fn can_proceed(&self, key: &str) -> bool {
        let bucket = self.buckets.entry(key.to_string()).or_insert_with(|| {
            TokenBucket {
                tokens: RwLock::new(self.config.security.rate_limiting.burst_size as f64),
                last_refill: RwLock::new(Instant::now()),
                capacity: self.config.security.rate_limiting.burst_size as f64,
                refill_rate: self.config.security.rate_limiting.requests_per_second as f64,
            }
        });
        
        bucket.consume_token()
    }
}

impl TokenBucket {
    fn consume_token(&self) -> bool {
        let now = Instant::now();
        let mut last_refill = self.last_refill.write();
        let mut tokens = self.tokens.write();
        
        let elapsed = now.duration_since(*last_refill).as_secs_f64();
        let new_tokens = (*tokens + elapsed * self.refill_rate).min(self.capacity);
        *tokens = new_tokens;
        *last_refill = now;
        
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[async_trait]
impl Middleware for RateLimitingMiddleware {
    async fn process_request(
        &self,
        req: &mut Request<Incoming>,
        client_addr: SocketAddr,
    ) -> Result<(), Response<String>> {
        let key = self.extract_key(req, client_addr);
        
        if !self.can_proceed(&key) {
            warn!("Rate limit exceeded for key: {}", key);
            return Err(Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .header("Retry-After", "60")
                .body("Rate limit exceeded".to_string())
                .unwrap());
        }
        
        Ok(())
    }
    
    async fn process_response(
        &self,
        _resp: &mut Response<String>,
        _client_addr: SocketAddr,
    ) -> Result<(), Response<String>> {
        Ok(())
    }
}

pub struct AuthenticationMiddleware {
    config: Arc<ProxyConfig>,
}

impl AuthenticationMiddleware {
    pub fn new(config: Arc<ProxyConfig>) -> Self {
        Self { config }
    }
    
    fn validate_jwt(&self, token: &str) -> bool {
        !token.is_empty() && token.len() > 10
    }
    
    fn validate_oauth2(&self, token: &str) -> bool {
        !token.is_empty() && token.starts_with("Bearer ")
    }
}

#[async_trait]
impl Middleware for AuthenticationMiddleware {
    async fn process_request(
        &self,
        req: &mut Request<Incoming>,
        _client_addr: SocketAddr,
    ) -> Result<(), Response<String>> {
        let auth_header = req.headers().get("authorization");
        
        let is_authenticated = match self.config.security.authentication.method.as_str() {
            "jwt" => {
                if let Some(auth) = auth_header {
                    if let Ok(auth_str) = auth.to_str() {
                        if let Some(token) = auth_str.strip_prefix("Bearer ") {
                            self.validate_jwt(token)
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            "oauth2" => {
                if let Some(auth) = auth_header {
                    if let Ok(auth_str) = auth.to_str() {
                        self.validate_oauth2(auth_str)
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            "mtls" => {
                req.headers().contains_key("x-client-cert")
            }
            _ => true,
        };
        
        if !is_authenticated {
            warn!("Authentication failed for request");
            return Err(Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .header("WWW-Authenticate", "Bearer")
                .body("Authentication required".to_string())
                .unwrap());
        }
        
        Ok(())
    }
    
    async fn process_response(
        &self,
        _resp: &mut Response<String>,
        _client_addr: SocketAddr,
    ) -> Result<(), Response<String>> {
        Ok(())
    }
}

pub struct AuthorizationMiddleware {
    config: Arc<ProxyConfig>,
}

impl AuthorizationMiddleware {
    pub fn new(config: Arc<ProxyConfig>) -> Self {
        Self { config }
    }
    
    fn check_authorization(&self, req: &Request<Incoming>) -> bool {
        let path = req.uri().path();
        let method = req.method().as_str();
        
        if path == "/health" || path == "/metrics" {
            return true;
        }
        
        for policy in &self.config.security.authorization.policies {
            if policy.contains(&format!("{}:{}", method, path)) {
                return true;
            }
        }
        
        false
    }
}

#[async_trait]
impl Middleware for AuthorizationMiddleware {
    async fn process_request(
        &self,
        req: &mut Request<Incoming>,
        _client_addr: SocketAddr,
    ) -> Result<(), Response<String>> {
        if !self.check_authorization(req) {
            warn!("Authorization failed for request: {} {}", req.method(), req.uri().path());
            return Err(Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body("Access denied".to_string())
                .unwrap());
        }
        
        Ok(())
    }
    
    async fn process_response(
        &self,
        _resp: &mut Response<String>,
        _client_addr: SocketAddr,
    ) -> Result<(), Response<String>> {
        Ok(())
    }
}

pub struct LoggingMiddleware {
    config: Arc<ProxyConfig>,
}

impl LoggingMiddleware {
    pub fn new(config: Arc<ProxyConfig>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Middleware for LoggingMiddleware {
    async fn process_request(
        &self,
        req: &mut Request<Incoming>,
        client_addr: SocketAddr,
    ) -> Result<(), Response<String>> {
        debug!(
            "Incoming request: {} {} from {}",
            req.method(),
            req.uri(),
            client_addr
        );
        
        let request_id = uuid::Uuid::new_v4().to_string();
        req.headers_mut().insert("x-request-id", request_id.parse().unwrap());
        
        Ok(())
    }
    
    async fn process_response(
        &self,
        resp: &mut Response<String>,
        client_addr: SocketAddr,
    ) -> Result<(), Response<String>> {
        debug!(
            "Outgoing response: {} to {}",
            resp.status(),
            client_addr
        );
        
        Ok(())
    }
}

pub struct MetricsMiddleware {
    config: Arc<ProxyConfig>,
    request_counter: Arc<RwLock<u64>>,
    response_times: Arc<RwLock<Vec<Duration>>>,
}

impl MetricsMiddleware {
    pub fn new(config: Arc<ProxyConfig>) -> Self {
        Self {
            config,
            request_counter: Arc::new(RwLock::new(0)),
            response_times: Arc::new(RwLock::new(Vec::new())),
        }
    }
}

#[async_trait]
impl Middleware for MetricsMiddleware {
    async fn process_request(
        &self,
        req: &mut Request<Incoming>,
        _client_addr: SocketAddr,
    ) -> Result<(), Response<String>> {
        let start_time = Instant::now();
        req.extensions_mut().insert(start_time);
        
        let mut counter = self.request_counter.write();
        *counter += 1;
        
        Ok(())
    }
    
    async fn process_response(
        &self,
        resp: &mut Response<String>,
        _client_addr: SocketAddr,
    ) -> Result<(), Response<String>> {
        if let Some(start_time) = resp.extensions().get::<Instant>() {
            let response_time = start_time.elapsed();
            let mut times = self.response_times.write();
            times.push(response_time);
            
            if times.len() > 1000 {
                times.remove(0);
            }
        }
        
        Ok(())
    }
}

pub struct CorsMiddleware {
    config: Arc<ProxyConfig>,
}

impl CorsMiddleware {
    pub fn new(config: Arc<ProxyConfig>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Middleware for CorsMiddleware {
    async fn process_request(
        &self,
        req: &mut Request<Incoming>,
        _client_addr: SocketAddr,
    ) -> Result<(), Response<String>> {
        if req.method() == hyper::Method::OPTIONS {
            return Err(Response::builder()
                .status(StatusCode::OK)
                .header("Access-Control-Allow-Origin", "*")
                .header("Access-Control-Allow-Methods", "GET, POST, PUT, DELETE, OPTIONS")
                .header("Access-Control-Allow-Headers", "Content-Type, Authorization")
                .header("Access-Control-Max-Age", "86400")
                .body(String::new())
                .unwrap());
        }
        
        Ok(())
    }
    
    async fn process_response(
        &self,
        resp: &mut Response<String>,
        _client_addr: SocketAddr,
    ) -> Result<(), Response<String>> {
        resp.headers_mut().insert("Access-Control-Allow-Origin", "*".parse().unwrap());
        resp.headers_mut().insert("Access-Control-Allow-Credentials", "true".parse().unwrap());
        
        Ok(())
    }
}
