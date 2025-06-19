use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::RwLock;
use dashmap::DashMap;
use tracing::{info, warn};

use crate::config::ProxyConfig;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Debug)]
pub struct CircuitBreakerState {
    state: RwLock<CircuitState>,
    failure_count: RwLock<u32>,
    last_failure_time: RwLock<Option<Instant>>,
    half_open_calls: RwLock<u32>,
    success_count: RwLock<u32>,
}

pub struct CircuitBreaker {
    config: Arc<ProxyConfig>,
    states: DashMap<String, Arc<CircuitBreakerState>>,
}

impl CircuitBreaker {
    pub fn new(config: Arc<ProxyConfig>) -> Self {
        Self {
            config,
            states: DashMap::new(),
        }
    }
    
    pub async fn can_execute(&self, service_name: &str) -> bool {
        if !self.config.circuit_breaker.enabled {
            return true;
        }
        
        let state = self.get_or_create_state(service_name);
        let current_state = *state.state.read();
        
        match current_state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                if let Some(last_failure) = *state.last_failure_time.read() {
                    let recovery_timeout = Duration::from_millis(self.config.circuit_breaker.recovery_timeout_ms);
                    if last_failure.elapsed() >= recovery_timeout {
                        *state.state.write() = CircuitState::HalfOpen;
                        *state.half_open_calls.write() = 0;
                        *state.success_count.write() = 0;
                        info!("Circuit breaker for {} transitioned to half-open", service_name);
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            CircuitState::HalfOpen => {
                let half_open_calls = *state.half_open_calls.read();
                if half_open_calls < self.config.circuit_breaker.half_open_max_calls {
                    *state.half_open_calls.write() = half_open_calls + 1;
                    true
                } else {
                    false
                }
            }
        }
    }
    
    pub async fn record_success(&self, service_name: &str) {
        if !self.config.circuit_breaker.enabled {
            return;
        }
        
        let state = self.get_or_create_state(service_name);
        let current_state = *state.state.read();
        
        match current_state {
            CircuitState::Closed => {
                *state.failure_count.write() = 0;
            }
            CircuitState::HalfOpen => {
                let success_count = {
                    let mut count = state.success_count.write();
                    *count += 1;
                    *count
                };
                
                if success_count >= self.config.circuit_breaker.half_open_max_calls {
                    *state.state.write() = CircuitState::Closed;
                    *state.failure_count.write() = 0;
                    *state.last_failure_time.write() = None;
                    info!("Circuit breaker for {} closed after successful recovery", service_name);
                }
            }
            CircuitState::Open => {
                warn!("Recorded success for {} while circuit breaker is open", service_name);
            }
        }
    }
    
    pub async fn record_failure(&self, service_name: &str) {
        if !self.config.circuit_breaker.enabled {
            return;
        }
        
        let state = self.get_or_create_state(service_name);
        let current_state = *state.state.read();
        
        match current_state {
            CircuitState::Closed => {
                let failure_count = {
                    let mut count = state.failure_count.write();
                    *count += 1;
                    *count
                };
                
                *state.last_failure_time.write() = Some(Instant::now());
                
                if failure_count >= self.config.circuit_breaker.failure_threshold {
                    *state.state.write() = CircuitState::Open;
                    warn!("Circuit breaker for {} opened after {} failures", service_name, failure_count);
                }
            }
            CircuitState::HalfOpen => {
                *state.state.write() = CircuitState::Open;
                *state.last_failure_time.write() = Some(Instant::now());
                *state.failure_count.write() = self.config.circuit_breaker.failure_threshold;
                warn!("Circuit breaker for {} reopened after failure during half-open state", service_name);
            }
            CircuitState::Open => {
                *state.last_failure_time.write() = Some(Instant::now());
            }
        }
    }
    
    pub fn get_state(&self, service_name: &str) -> Option<CircuitState> {
        self.states.get(service_name).map(|state| *state.state.read())
    }
    
    pub fn get_failure_count(&self, service_name: &str) -> u32 {
        self.states
            .get(service_name)
            .map(|state| *state.failure_count.read())
            .unwrap_or(0)
    }
    
    pub fn reset(&self, service_name: &str) {
        if let Some(state) = self.states.get(service_name) {
            *state.state.write() = CircuitState::Closed;
            *state.failure_count.write() = 0;
            *state.last_failure_time.write() = None;
            *state.half_open_calls.write() = 0;
            *state.success_count.write() = 0;
            info!("Circuit breaker for {} manually reset", service_name);
        }
    }
    
    fn get_or_create_state(&self, service_name: &str) -> Arc<CircuitBreakerState> {
        self.states
            .entry(service_name.to_string())
            .or_insert_with(|| {
                Arc::new(CircuitBreakerState {
                    state: RwLock::new(CircuitState::Closed),
                    failure_count: RwLock::new(0),
                    last_failure_time: RwLock::new(None),
                    half_open_calls: RwLock::new(0),
                    success_count: RwLock::new(0),
                })
            })
            .clone()
    }
    
    pub fn get_all_states(&self) -> Vec<(String, CircuitState, u32)> {
        self.states
            .iter()
            .map(|entry| {
                let service_name = entry.key().clone();
                let state = entry.value();
                let current_state = *state.state.read();
                let failure_count = *state.failure_count.read();
                (service_name, current_state, failure_count)
            })
            .collect()
    }
}

impl Default for CircuitBreakerState {
    fn default() -> Self {
        Self {
            state: RwLock::new(CircuitState::Closed),
            failure_count: RwLock::new(0),
            last_failure_time: RwLock::new(None),
            half_open_calls: RwLock::new(0),
            success_count: RwLock::new(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CircuitBreakerConfig, ProxyConfig};
    
    fn create_test_config() -> Arc<ProxyConfig> {
        let mut config = ProxyConfig::default_config();
        config.circuit_breaker = CircuitBreakerConfig {
            enabled: true,
            failure_threshold: 3,
            recovery_timeout_ms: 1000,
            half_open_max_calls: 2,
        };
        Arc::new(config)
    }
    
    #[tokio::test]
    async fn test_circuit_breaker_closed_state() {
        let config = create_test_config();
        let cb = CircuitBreaker::new(config);
        
        assert!(cb.can_execute("test-service").await);
        assert_eq!(cb.get_state("test-service"), Some(CircuitState::Closed));
    }
    
    #[tokio::test]
    async fn test_circuit_breaker_opens_after_failures() {
        let config = create_test_config();
        let cb = CircuitBreaker::new(config);
        
        for _ in 0..3 {
            cb.record_failure("test-service").await;
        }
        
        assert_eq!(cb.get_state("test-service"), Some(CircuitState::Open));
        assert!(!cb.can_execute("test-service").await);
    }
    
    #[tokio::test]
    async fn test_circuit_breaker_half_open_transition() {
        let config = create_test_config();
        let cb = CircuitBreaker::new(config);
        
        for _ in 0..3 {
            cb.record_failure("test-service").await;
        }
        
        tokio::time::sleep(Duration::from_millis(1100)).await;
        
        assert!(cb.can_execute("test-service").await);
        assert_eq!(cb.get_state("test-service"), Some(CircuitState::HalfOpen));
    }
}
