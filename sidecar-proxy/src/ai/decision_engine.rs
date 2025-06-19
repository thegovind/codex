use std::sync::Arc;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};
use anyhow::Result;

use crate::config::ProxyConfig;
use super::agent::{SystemHealth, AiDecision, DecisionType, Action, AlertSeverity, Anomaly, AnomalyType};

pub struct DecisionEngine {
    config: Arc<ProxyConfig>,
    rules: Vec<DecisionRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRule {
    pub name: String,
    pub condition: Condition,
    pub decision_type: DecisionType,
    pub actions: Vec<Action>,
    pub confidence: f64,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Condition {
    ErrorRateAbove(f64),
    ResponseTimeAbove(f64),
    RequestRateAbove(u64),
    UpstreamUnhealthy(String),
    AnomalyDetected(AnomalyType),
    And(Vec<Condition>),
    Or(Vec<Condition>),
}

impl DecisionEngine {
    pub async fn new(config: Arc<ProxyConfig>) -> Result<Self> {
        let rules = Self::create_default_rules();
        
        Ok(Self {
            config,
            rules,
        })
    }
    
    pub async fn analyze_health(&self, health: &SystemHealth) -> Option<AiDecision> {
        for rule in &self.rules {
            if !rule.enabled {
                continue;
            }
            
            if self.evaluate_condition(&rule.condition, health) {
                debug!("Decision rule '{}' triggered", rule.name);
                
                return Some(AiDecision {
                    decision_type: rule.decision_type.clone(),
                    confidence: rule.confidence,
                    reasoning: format!("Rule '{}' triggered based on system health", rule.name),
                    actions: rule.actions.clone(),
                    timestamp: chrono::Utc::now(),
                });
            }
        }
        
        None
    }
    
    pub async fn handle_anomaly(&self, anomaly: Anomaly) -> Option<AiDecision> {
        let decision_type = match anomaly.anomaly_type {
            AnomalyType::HighLatency => DecisionType::AdjustRateLimit,
            AnomalyType::UnusualTrafficPattern => DecisionType::SecurityAlert,
            AnomalyType::ErrorSpike => DecisionType::CircuitBreakerOpen,
            AnomalyType::SecurityThreat => DecisionType::BlockTraffic,
            AnomalyType::ResourceExhaustion => DecisionType::ScaleUp,
        };
        
        let actions = match anomaly.anomaly_type {
            AnomalyType::HighLatency => vec![
                Action::AdjustWeight {
                    service: "default".to_string(),
                    endpoint: "slow-endpoint".to_string(),
                    weight: 10,
                },
                Action::AlertOperator {
                    message: format!("High latency detected: {}", anomaly.description),
                    severity: AlertSeverity::Medium,
                },
            ],
            AnomalyType::UnusualTrafficPattern => vec![
                Action::UpdateRateLimit { requests_per_second: 100 },
                Action::AlertOperator {
                    message: format!("Unusual traffic pattern: {}", anomaly.description),
                    severity: AlertSeverity::High,
                },
            ],
            AnomalyType::ErrorSpike => vec![
                Action::OpenCircuitBreaker { service: "default".to_string() },
                Action::AlertOperator {
                    message: format!("Error spike detected: {}", anomaly.description),
                    severity: AlertSeverity::High,
                },
            ],
            AnomalyType::SecurityThreat => vec![
                Action::BlockIp {
                    ip: "suspicious-ip".to_string(),
                    duration_seconds: 3600,
                },
                Action::AlertOperator {
                    message: format!("Security threat detected: {}", anomaly.description),
                    severity: AlertSeverity::Critical,
                },
            ],
            AnomalyType::ResourceExhaustion => vec![
                Action::AlertOperator {
                    message: format!("Resource exhaustion: {}", anomaly.description),
                    severity: AlertSeverity::Critical,
                },
            ],
        };
        
        Some(AiDecision {
            decision_type,
            confidence: anomaly.severity,
            reasoning: format!("Anomaly detected: {}", anomaly.description),
            actions,
            timestamp: chrono::Utc::now(),
        })
    }
    
    fn evaluate_condition(&self, condition: &Condition, health: &SystemHealth) -> bool {
        match condition {
            Condition::ErrorRateAbove(threshold) => health.error_rate > *threshold,
            Condition::ResponseTimeAbove(threshold) => health.avg_response_time > *threshold,
            Condition::RequestRateAbove(threshold) => {
                health.total_requests > *threshold
            }
            Condition::UpstreamUnhealthy(service) => {
                health.upstream_health.iter()
                    .any(|(name, healthy)| name == service && !healthy)
            }
            Condition::AnomalyDetected(_) => false, // Handled separately
            Condition::And(conditions) => {
                conditions.iter().all(|c| self.evaluate_condition(c, health))
            }
            Condition::Or(conditions) => {
                conditions.iter().any(|c| self.evaluate_condition(c, health))
            }
        }
    }
    
    fn create_default_rules() -> Vec<DecisionRule> {
        vec![
            DecisionRule {
                name: "High Error Rate".to_string(),
                condition: Condition::ErrorRateAbove(0.05), // 5% error rate
                decision_type: DecisionType::CircuitBreakerOpen,
                actions: vec![
                    Action::OpenCircuitBreaker { service: "default".to_string() },
                    Action::AlertOperator {
                        message: "High error rate detected, opening circuit breaker".to_string(),
                        severity: AlertSeverity::High,
                    },
                ],
                confidence: 0.9,
                enabled: true,
            },
            DecisionRule {
                name: "High Response Time".to_string(),
                condition: Condition::ResponseTimeAbove(5000.0), // 5 seconds
                decision_type: DecisionType::AdjustRateLimit,
                actions: vec![
                    Action::UpdateRateLimit { requests_per_second: 100 },
                    Action::AlertOperator {
                        message: "High response time detected, reducing rate limit".to_string(),
                        severity: AlertSeverity::Medium,
                    },
                ],
                confidence: 0.8,
                enabled: true,
            },
            DecisionRule {
                name: "Upstream Unhealthy".to_string(),
                condition: Condition::UpstreamUnhealthy("default".to_string()),
                decision_type: DecisionType::RouteTraffic,
                actions: vec![
                    Action::AlertOperator {
                        message: "Upstream service unhealthy, rerouting traffic".to_string(),
                        severity: AlertSeverity::High,
                    },
                ],
                confidence: 0.95,
                enabled: true,
            },
            DecisionRule {
                name: "Critical System State".to_string(),
                condition: Condition::And(vec![
                    Condition::ErrorRateAbove(0.1),
                    Condition::ResponseTimeAbove(10000.0),
                ]),
                decision_type: DecisionType::SecurityAlert,
                actions: vec![
                    Action::AlertOperator {
                        message: "Critical system state: high error rate and response time".to_string(),
                        severity: AlertSeverity::Critical,
                    },
                ],
                confidence: 0.99,
                enabled: true,
            },
        ]
    }
    
    pub fn add_rule(&mut self, rule: DecisionRule) {
        info!("Adding decision rule: {}", rule.name);
        self.rules.push(rule);
    }
    
    pub fn remove_rule(&mut self, name: &str) -> bool {
        if let Some(pos) = self.rules.iter().position(|r| r.name == name) {
            self.rules.remove(pos);
            info!("Removed decision rule: {}", name);
            true
        } else {
            false
        }
    }
    
    pub fn enable_rule(&mut self, name: &str) -> bool {
        if let Some(rule) = self.rules.iter_mut().find(|r| r.name == name) {
            rule.enabled = true;
            info!("Enabled decision rule: {}", name);
            true
        } else {
            false
        }
    }
    
    pub fn disable_rule(&mut self, name: &str) -> bool {
        if let Some(rule) = self.rules.iter_mut().find(|r| r.name == name) {
            rule.enabled = false;
            info!("Disabled decision rule: {}", name);
            true
        } else {
            false
        }
    }
    
    pub fn get_rules(&self) -> &[DecisionRule] {
        &self.rules
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProxyConfig;
    
    fn create_test_health() -> SystemHealth {
        SystemHealth {
            total_requests: 1000,
            successful_requests: 950,
            failed_requests: 50,
            avg_response_time: 200.0,
            error_rate: 0.05,
            active_connections: 100,
            upstream_health: vec![("default".to_string(), true)],
            timestamp: chrono::Utc::now(),
        }
    }
    
    #[tokio::test]
    async fn test_error_rate_rule() {
        let config = Arc::new(ProxyConfig::default_config());
        let engine = DecisionEngine::new(config).await.unwrap();
        
        let mut health = create_test_health();
        health.error_rate = 0.06; // Above 5% threshold
        
        let decision = engine.analyze_health(&health).await;
        assert!(decision.is_some());
        
        let decision = decision.unwrap();
        assert!(matches!(decision.decision_type, DecisionType::CircuitBreakerOpen));
    }
    
    #[tokio::test]
    async fn test_response_time_rule() {
        let config = Arc::new(ProxyConfig::default_config());
        let engine = DecisionEngine::new(config).await.unwrap();
        
        let mut health = create_test_health();
        health.avg_response_time = 6000.0; // Above 5 second threshold
        
        let decision = engine.analyze_health(&health).await;
        assert!(decision.is_some());
        
        let decision = decision.unwrap();
        assert!(matches!(decision.decision_type, DecisionType::AdjustRateLimit));
    }
    
    #[tokio::test]
    async fn test_no_rule_triggered() {
        let config = Arc::new(ProxyConfig::default_config());
        let engine = DecisionEngine::new(config).await.unwrap();
        
        let health = create_test_health(); // Normal health
        
        let decision = engine.analyze_health(&health).await;
        assert!(decision.is_none());
    }
}
