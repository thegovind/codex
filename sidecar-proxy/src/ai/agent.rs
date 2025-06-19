use std::sync::Arc;
use std::time::Duration;
use serde::{Deserialize, Serialize};
use tracing::{info, warn, error, debug};
use anyhow::Result;
use hyper::{Request, Response};
use hyper::body::Incoming;
use reqwest::Client;
use tokio::time::interval;

use crate::config::ProxyConfig;
use crate::proxy::server::ProxyMetrics;
use super::decision_engine::DecisionEngine;
use super::learning::LearningEngine;
use super::anomaly_detection::AnomalyDetector;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemHealth {
    pub total_requests: u64,
    pub successful_requests: u64,
    pub failed_requests: u64,
    pub avg_response_time: f64,
    pub error_rate: f64,
    pub active_connections: u64,
    pub upstream_health: Vec<(String, bool)>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiDecision {
    pub decision_type: DecisionType,
    pub confidence: f64,
    pub reasoning: String,
    pub actions: Vec<Action>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DecisionType {
    ScaleUp,
    ScaleDown,
    CircuitBreakerOpen,
    CircuitBreakerClose,
    RouteTraffic,
    BlockTraffic,
    AdjustRateLimit,
    HealthCheckAdjustment,
    LoadBalancingChange,
    SecurityAlert,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Action {
    AddEndpoint { service: String, endpoint: String },
    RemoveEndpoint { service: String, endpoint: String },
    AdjustWeight { service: String, endpoint: String, weight: u32 },
    UpdateRateLimit { requests_per_second: u32 },
    OpenCircuitBreaker { service: String },
    CloseCircuitBreaker { service: String },
    BlockIp { ip: String, duration_seconds: u64 },
    AlertOperator { message: String, severity: AlertSeverity },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AlertSeverity {
    Low,
    Medium,
    High,
    Critical,
}

pub struct AiAgent {
    config: Arc<ProxyConfig>,
    decision_engine: DecisionEngine,
    learning_engine: LearningEngine,
    anomaly_detector: AnomalyDetector,
    http_client: Client,
    decision_history: Arc<parking_lot::RwLock<Vec<AiDecision>>>,
}

impl AiAgent {
    pub async fn new(config: Arc<ProxyConfig>) -> Result<Self> {
        let decision_engine = DecisionEngine::new(config.clone()).await?;
        let learning_engine = LearningEngine::new(config.clone()).await?;
        let anomaly_detector = AnomalyDetector::new(config.clone()).await?;
        
        let http_client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        
        Ok(Self {
            config,
            decision_engine,
            learning_engine,
            anomaly_detector,
            http_client,
            decision_history: Arc::new(parking_lot::RwLock::new(Vec::new())),
        })
    }
    
    pub async fn analyze_request_response(
        &self,
        req: &Request<Incoming>,
        resp: &Response<String>,
        duration: Duration,
    ) {
        let features = self.extract_features(req, resp, duration).await;
        
        if let Some(anomaly) = self.anomaly_detector.detect_anomaly(&features).await {
            warn!("Anomaly detected: {:?}", anomaly);
            
            if let Some(decision) = self.decision_engine.handle_anomaly(anomaly).await {
                self.execute_decision(decision).await;
            }
        }
        
        self.learning_engine.update_model(&features).await;
    }
    
    pub async fn analyze_system_health(&self, metrics: &ProxyMetrics) {
        let health = self.collect_system_health(metrics).await;
        
        debug!("System health: error_rate={:.2}%, avg_response_time={:.2}ms", 
               health.error_rate * 100.0, health.avg_response_time);
        
        if let Some(decision) = self.decision_engine.analyze_health(&health).await {
            info!("AI decision made: {:?}", decision.decision_type);
            
            if decision.confidence >= self.config.ai.decision_threshold {
                if self.config.ai.autonomous_mode {
                    self.execute_decision(decision).await;
                } else {
                    self.recommend_decision(decision).await;
                }
            }
        }
        
        if let Some(anomaly) = self.anomaly_detector.detect_system_anomaly(&health).await {
            warn!("System anomaly detected: {:?}", anomaly);
            
            let decision = AiDecision {
                decision_type: DecisionType::SecurityAlert,
                confidence: 0.9,
                reasoning: format!("System anomaly detected: {:?}", anomaly),
                actions: vec![Action::AlertOperator {
                    message: format!("System anomaly: {:?}", anomaly),
                    severity: AlertSeverity::High,
                }],
                timestamp: chrono::Utc::now(),
            };
            
            self.execute_decision(decision).await;
        }
    }
    
    async fn collect_system_health(&self, metrics: &ProxyMetrics) -> SystemHealth {
        let total_requests = *metrics.total_requests.read();
        let successful_requests = *metrics.successful_requests.read();
        let failed_requests = *metrics.failed_requests.read();
        
        let error_rate = if total_requests > 0 {
            failed_requests as f64 / total_requests as f64
        } else {
            0.0
        };
        
        let avg_response_time = {
            let times = metrics.response_times.read();
            if !times.is_empty() {
                times.iter().map(|t| *t as f64).sum::<f64>() / times.len() as f64
            } else {
                0.0
            }
        };
        
        let upstream_health: Vec<(String, bool)> = metrics
            .upstream_health
            .iter()
            .map(|entry| (entry.key().clone(), *entry.value()))
            .collect();
        
        SystemHealth {
            total_requests,
            successful_requests,
            failed_requests,
            avg_response_time,
            error_rate,
            active_connections: 0, // TODO: Get from connection tracker
            upstream_health,
            timestamp: chrono::Utc::now(),
        }
    }
    
    async fn extract_features(
        &self,
        req: &Request<Incoming>,
        resp: &Response<String>,
        duration: Duration,
    ) -> RequestFeatures {
        RequestFeatures {
            method: req.method().to_string(),
            path: req.uri().path().to_string(),
            status_code: resp.status().as_u16(),
            response_time_ms: duration.as_millis() as u64,
            request_size: req.headers()
                .get("content-length")
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            response_size: resp.headers()
                .get("content-length")
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            user_agent: req.headers()
                .get("user-agent")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("")
                .to_string(),
            timestamp: chrono::Utc::now(),
        }
    }
    
    async fn execute_decision(&self, decision: AiDecision) {
        info!("Executing AI decision: {:?} (confidence: {:.2})", 
              decision.decision_type, decision.confidence);
        
        {
            let mut history = self.decision_history.write();
            history.push(decision.clone());
            
            if history.len() > 1000 {
                history.remove(0);
            }
        }
        
        for action in &decision.actions {
            if let Err(e) = self.execute_action(action).await {
                error!("Failed to execute action {:?}: {}", action, e);
            }
        }
        
        if let Some(endpoint) = &self.config.ai.endpoint {
            if let Err(e) = self.send_decision_to_ai_service(endpoint, &decision).await {
                warn!("Failed to send decision to AI service: {}", e);
            }
        }
    }
    
    async fn execute_action(&self, action: &Action) -> Result<()> {
        match action {
            Action::AddEndpoint { service, endpoint } => {
                info!("Adding endpoint {} to service {}", endpoint, service);
            }
            Action::RemoveEndpoint { service, endpoint } => {
                info!("Removing endpoint {} from service {}", endpoint, service);
            }
            Action::AdjustWeight { service, endpoint, weight } => {
                info!("Adjusting weight of endpoint {} in service {} to {}", 
                      endpoint, service, weight);
            }
            Action::UpdateRateLimit { requests_per_second } => {
                info!("Updating rate limit to {} requests/second", requests_per_second);
            }
            Action::OpenCircuitBreaker { service } => {
                info!("Opening circuit breaker for service {}", service);
            }
            Action::CloseCircuitBreaker { service } => {
                info!("Closing circuit breaker for service {}", service);
            }
            Action::BlockIp { ip, duration_seconds } => {
                warn!("Blocking IP {} for {} seconds", ip, duration_seconds);
            }
            Action::AlertOperator { message, severity } => {
                match severity {
                    AlertSeverity::Critical => error!("CRITICAL ALERT: {}", message),
                    AlertSeverity::High => warn!("HIGH ALERT: {}", message),
                    AlertSeverity::Medium => warn!("MEDIUM ALERT: {}", message),
                    AlertSeverity::Low => info!("LOW ALERT: {}", message),
                }
            }
        }
        
        Ok(())
    }
    
    async fn recommend_decision(&self, decision: AiDecision) {
        info!("AI recommendation: {:?} (confidence: {:.2})", 
              decision.decision_type, decision.confidence);
        info!("Reasoning: {}", decision.reasoning);
        
    }
    
    async fn send_decision_to_ai_service(&self, endpoint: &str, decision: &AiDecision) -> Result<()> {
        let response = self.http_client
            .post(endpoint)
            .json(decision)
            .send()
            .await?;
        
        if !response.status().is_success() {
            return Err(anyhow::anyhow!("AI service returned error: {}", response.status()));
        }
        
        Ok(())
    }
    
    pub fn get_decision_history(&self) -> Vec<AiDecision> {
        self.decision_history.read().clone()
    }
    
    pub async fn start_monitoring_loop(&self) {
        let mut interval = interval(Duration::from_secs(30));
        
        loop {
            interval.tick().await;
            
            debug!("AI agent monitoring tick");
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestFeatures {
    pub method: String,
    pub path: String,
    pub status_code: u16,
    pub response_time_ms: u64,
    pub request_size: u64,
    pub response_size: u64,
    pub user_agent: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Anomaly {
    pub anomaly_type: AnomalyType,
    pub severity: f64,
    pub description: String,
    pub features: RequestFeatures,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AnomalyType {
    HighLatency,
    UnusualTrafficPattern,
    ErrorSpike,
    SecurityThreat,
    ResourceExhaustion,
}
