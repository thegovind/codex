use std::sync::Arc;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
use anyhow::Result;
use parking_lot::RwLock;
use std::collections::{HashMap, VecDeque};

use crate::config::ProxyConfig;
use super::agent::{RequestFeatures, SystemHealth, Anomaly, AnomalyType};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnomalyThresholds {
    pub response_time_percentile_99: f64,
    pub error_rate_threshold: f64,
    pub request_rate_threshold: f64,
    pub unusual_pattern_threshold: f64,
    pub security_threat_threshold: f64,
}

#[derive(Debug, Clone)]
struct TimeSeriesData {
    timestamps: VecDeque<chrono::DateTime<chrono::Utc>>,
    values: VecDeque<f64>,
    max_size: usize,
}

pub struct AnomalyDetector {
    config: Arc<ProxyConfig>,
    thresholds: RwLock<AnomalyThresholds>,
    response_times: RwLock<TimeSeriesData>,
    error_rates: RwLock<TimeSeriesData>,
    request_rates: RwLock<TimeSeriesData>,
    path_patterns: RwLock<HashMap<String, PathStatistics>>,
    ip_patterns: RwLock<HashMap<String, IpStatistics>>,
}

#[derive(Debug, Clone)]
struct PathStatistics {
    request_count: u64,
    avg_response_time: f64,
    error_count: u64,
    last_seen: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
struct IpStatistics {
    request_count: u64,
    error_count: u64,
    blocked_count: u64,
    first_seen: chrono::DateTime<chrono::Utc>,
    last_seen: chrono::DateTime<chrono::Utc>,
    user_agents: HashMap<String, u64>,
}

impl AnomalyDetector {
    pub async fn new(config: Arc<ProxyConfig>) -> Result<Self> {
        let thresholds = AnomalyThresholds {
            response_time_percentile_99: 5000.0, // 5 seconds
            error_rate_threshold: 0.05, // 5%
            request_rate_threshold: 1000.0, // requests per minute
            unusual_pattern_threshold: 0.8,
            security_threat_threshold: 0.9,
        };
        
        Ok(Self {
            config,
            thresholds: RwLock::new(thresholds),
            response_times: RwLock::new(TimeSeriesData::new(1000)),
            error_rates: RwLock::new(TimeSeriesData::new(100)),
            request_rates: RwLock::new(TimeSeriesData::new(100)),
            path_patterns: RwLock::new(HashMap::new()),
            ip_patterns: RwLock::new(HashMap::new()),
        })
    }
    
    pub async fn detect_anomaly(&self, features: &RequestFeatures) -> Option<Anomaly> {
        self.update_statistics(features).await;
        
        if let Some(anomaly) = self.detect_high_latency(features).await {
            return Some(anomaly);
        }
        
        if let Some(anomaly) = self.detect_unusual_traffic_pattern(features).await {
            return Some(anomaly);
        }
        
        if let Some(anomaly) = self.detect_security_threat(features).await {
            return Some(anomaly);
        }
        
        None
    }
    
    pub async fn detect_system_anomaly(&self, health: &SystemHealth) -> Option<Anomaly> {
        let thresholds = self.thresholds.read();
        
        if health.error_rate > thresholds.error_rate_threshold {
            return Some(Anomaly {
                anomaly_type: AnomalyType::ErrorSpike,
                severity: (health.error_rate / thresholds.error_rate_threshold).min(1.0),
                description: format!("Error rate spike: {:.2}% (threshold: {:.2}%)", 
                                   health.error_rate * 100.0, 
                                   thresholds.error_rate_threshold * 100.0),
                features: self.create_dummy_features(), // System-level anomaly
            });
        }
        
        if health.avg_response_time > thresholds.response_time_percentile_99 {
            return Some(Anomaly {
                anomaly_type: AnomalyType::HighLatency,
                severity: (health.avg_response_time / thresholds.response_time_percentile_99).min(1.0),
                description: format!("High average response time: {:.2}ms (threshold: {:.2}ms)", 
                                   health.avg_response_time, 
                                   thresholds.response_time_percentile_99),
                features: self.create_dummy_features(),
            });
        }
        
        let unhealthy_upstreams = health.upstream_health.iter()
            .filter(|(_, healthy)| !healthy)
            .count();
        
        if unhealthy_upstreams > 0 {
            let total_upstreams = health.upstream_health.len();
            let unhealthy_ratio = unhealthy_upstreams as f64 / total_upstreams as f64;
            
            if unhealthy_ratio > 0.5 {
                return Some(Anomaly {
                    anomaly_type: AnomalyType::ResourceExhaustion,
                    severity: unhealthy_ratio,
                    description: format!("Multiple upstream services unhealthy: {}/{}", 
                                       unhealthy_upstreams, total_upstreams),
                    features: self.create_dummy_features(),
                });
            }
        }
        
        None
    }
    
    async fn update_statistics(&self, features: &RequestFeatures) {
        let now = chrono::Utc::now();
        
        {
            let mut response_times = self.response_times.write();
            response_times.add_point(now, features.response_time_ms as f64);
        }
        
        {
            let mut path_patterns = self.path_patterns.write();
            let path_stats = path_patterns.entry(features.path.clone()).or_insert(PathStatistics {
                request_count: 0,
                avg_response_time: 0.0,
                error_count: 0,
                last_seen: now,
            });
            
            path_stats.request_count += 1;
            path_stats.avg_response_time = (path_stats.avg_response_time * (path_stats.request_count - 1) as f64 
                + features.response_time_ms as f64) / path_stats.request_count as f64;
            
            if features.status_code >= 400 {
                path_stats.error_count += 1;
            }
            
            path_stats.last_seen = now;
        }
        
        let client_ip = "unknown".to_string(); // Would extract from request
        {
            let mut ip_patterns = self.ip_patterns.write();
            let ip_stats = ip_patterns.entry(client_ip).or_insert(IpStatistics {
                request_count: 0,
                error_count: 0,
                blocked_count: 0,
                first_seen: now,
                last_seen: now,
                user_agents: HashMap::new(),
            });
            
            ip_stats.request_count += 1;
            ip_stats.last_seen = now;
            
            if features.status_code >= 400 {
                ip_stats.error_count += 1;
            }
            
            *ip_stats.user_agents.entry(features.user_agent.clone()).or_insert(0) += 1;
        }
    }
    
    async fn detect_high_latency(&self, features: &RequestFeatures) -> Option<Anomaly> {
        let thresholds = self.thresholds.read();
        
        if features.response_time_ms as f64 > thresholds.response_time_percentile_99 {
            let severity = (features.response_time_ms as f64 / thresholds.response_time_percentile_99).min(1.0);
            
            return Some(Anomaly {
                anomaly_type: AnomalyType::HighLatency,
                severity,
                description: format!("High response time: {}ms for {} {}", 
                                   features.response_time_ms, 
                                   features.method, 
                                   features.path),
                features: features.clone(),
            });
        }
        
        None
    }
    
    async fn detect_unusual_traffic_pattern(&self, features: &RequestFeatures) -> Option<Anomaly> {
        let path_patterns = self.path_patterns.read();
        
        if let Some(path_stats) = path_patterns.get(&features.path) {
            let response_time_ratio = features.response_time_ms as f64 / path_stats.avg_response_time;
            
            if response_time_ratio > 5.0 && path_stats.request_count > 10 {
                return Some(Anomaly {
                    anomaly_type: AnomalyType::UnusualTrafficPattern,
                    severity: (response_time_ratio / 10.0).min(1.0),
                    description: format!("Unusual response time for path {}: {}ms vs avg {}ms", 
                                       features.path, 
                                       features.response_time_ms, 
                                       path_stats.avg_response_time),
                    features: features.clone(),
                });
            }
        }
        
        None
    }
    
    async fn detect_security_threat(&self, features: &RequestFeatures) -> Option<Anomaly> {
        let path = features.path.to_lowercase();
        let user_agent = features.user_agent.to_lowercase();
        
        if path.contains("union") && path.contains("select") ||
           path.contains("'") && path.contains("or") && path.contains("1=1") {
            return Some(Anomaly {
                anomaly_type: AnomalyType::SecurityThreat,
                severity: 0.9,
                description: format!("Potential SQL injection attempt: {}", features.path),
                features: features.clone(),
            });
        }
        
        if path.contains("<script") || path.contains("javascript:") || path.contains("onerror=") {
            return Some(Anomaly {
                anomaly_type: AnomalyType::SecurityThreat,
                severity: 0.8,
                description: format!("Potential XSS attempt: {}", features.path),
                features: features.clone(),
            });
        }
        
        if path.contains("../") || path.contains("..\\") {
            return Some(Anomaly {
                anomaly_type: AnomalyType::SecurityThreat,
                severity: 0.7,
                description: format!("Potential directory traversal attempt: {}", features.path),
                features: features.clone(),
            });
        }
        
        if user_agent.contains("sqlmap") || user_agent.contains("nikto") || 
           user_agent.contains("nmap") || user_agent.contains("masscan") {
            return Some(Anomaly {
                anomaly_type: AnomalyType::SecurityThreat,
                severity: 0.95,
                description: format!("Suspicious user agent: {}", features.user_agent),
                features: features.clone(),
            });
        }
        
        None
    }
    
    fn create_dummy_features(&self) -> RequestFeatures {
        RequestFeatures {
            method: "SYSTEM".to_string(),
            path: "/system/health".to_string(),
            status_code: 200,
            response_time_ms: 0,
            request_size: 0,
            response_size: 0,
            user_agent: "system".to_string(),
            timestamp: chrono::Utc::now(),
        }
    }
    
    pub fn update_thresholds(&self, thresholds: AnomalyThresholds) {
        *self.thresholds.write() = thresholds;
        debug!("Updated anomaly detection thresholds");
    }
    
    pub fn get_statistics(&self) -> AnomalyDetectionStats {
        let path_patterns = self.path_patterns.read();
        let ip_patterns = self.ip_patterns.read();
        
        AnomalyDetectionStats {
            monitored_paths: path_patterns.len(),
            monitored_ips: ip_patterns.len(),
            response_time_samples: self.response_times.read().values.len(),
            thresholds: self.thresholds.read().clone(),
        }
    }
}

impl TimeSeriesData {
    fn new(max_size: usize) -> Self {
        Self {
            timestamps: VecDeque::with_capacity(max_size),
            values: VecDeque::with_capacity(max_size),
            max_size,
        }
    }
    
    fn add_point(&mut self, timestamp: chrono::DateTime<chrono::Utc>, value: f64) {
        if self.timestamps.len() >= self.max_size {
            self.timestamps.pop_front();
            self.values.pop_front();
        }
        
        self.timestamps.push_back(timestamp);
        self.values.push_back(value);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnomalyDetectionStats {
    pub monitored_paths: usize,
    pub monitored_ips: usize,
    pub response_time_samples: usize,
    pub thresholds: AnomalyThresholds,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProxyConfig;
    
    fn create_test_features() -> RequestFeatures {
        RequestFeatures {
            method: "GET".to_string(),
            path: "/api/test".to_string(),
            status_code: 200,
            response_time_ms: 100,
            request_size: 1024,
            response_size: 2048,
            user_agent: "test-agent".to_string(),
            timestamp: chrono::Utc::now(),
        }
    }
    
    #[tokio::test]
    async fn test_anomaly_detector_creation() {
        let config = Arc::new(ProxyConfig::default_config());
        let detector = AnomalyDetector::new(config).await.unwrap();
        
        let stats = detector.get_statistics();
        assert_eq!(stats.monitored_paths, 0);
        assert_eq!(stats.monitored_ips, 0);
    }
    
    #[tokio::test]
    async fn test_high_latency_detection() {
        let config = Arc::new(ProxyConfig::default_config());
        let detector = AnomalyDetector::new(config).await.unwrap();
        
        let mut features = create_test_features();
        features.response_time_ms = 10000; // 10 seconds - above threshold
        
        let anomaly = detector.detect_anomaly(&features).await;
        assert!(anomaly.is_some());
        
        let anomaly = anomaly.unwrap();
        assert!(matches!(anomaly.anomaly_type, AnomalyType::HighLatency));
    }
    
    #[tokio::test]
    async fn test_security_threat_detection() {
        let config = Arc::new(ProxyConfig::default_config());
        let detector = AnomalyDetector::new(config).await.unwrap();
        
        let mut features = create_test_features();
        features.path = "/api/test?id=1' OR '1'='1".to_string();
        
        let anomaly = detector.detect_anomaly(&features).await;
        assert!(anomaly.is_some());
        
        let anomaly = anomaly.unwrap();
        assert!(matches!(anomaly.anomaly_type, AnomalyType::SecurityThreat));
    }
    
    #[tokio::test]
    async fn test_normal_request_no_anomaly() {
        let config = Arc::new(ProxyConfig::default_config());
        let detector = AnomalyDetector::new(config).await.unwrap();
        
        let features = create_test_features();
        let anomaly = detector.detect_anomaly(&features).await;
        assert!(anomaly.is_none());
    }
}
