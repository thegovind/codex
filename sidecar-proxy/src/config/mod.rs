use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use anyhow::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub server: ServerConfig,
    pub upstream: UpstreamConfig,
    pub load_balancing: LoadBalancingConfig,
    pub circuit_breaker: CircuitBreakerConfig,
    pub retry: RetryConfig,
    pub observability: ObservabilityConfig,
    pub ai: AiConfig,
    pub security: SecurityConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub bind_address: String,
    pub port: u16,
    pub tls: Option<TlsConfig>,
    pub max_connections: usize,
    pub connection_timeout_ms: u64,
    pub request_timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    pub cert_path: String,
    pub key_path: String,
    pub ca_path: Option<String>,
    pub verify_client: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamConfig {
    pub services: HashMap<String, ServiceConfig>,
    pub discovery: ServiceDiscoveryConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub endpoints: Vec<EndpointConfig>,
    pub health_check: HealthCheckConfig,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointConfig {
    pub address: String,
    pub port: u16,
    pub weight: u32,
    pub metadata: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceDiscoveryConfig {
    pub provider: String, // "static", "consul", "etcd", "kubernetes"
    pub config: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckConfig {
    pub enabled: bool,
    pub path: String,
    pub interval_ms: u64,
    pub timeout_ms: u64,
    pub healthy_threshold: u32,
    pub unhealthy_threshold: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadBalancingConfig {
    pub algorithm: String, // "round_robin", "least_connections", "weighted_round_robin", "consistent_hash"
    pub sticky_sessions: bool,
    pub session_affinity_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerConfig {
    pub enabled: bool,
    pub failure_threshold: u32,
    pub recovery_timeout_ms: u64,
    pub half_open_max_calls: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    pub enabled: bool,
    pub max_attempts: u32,
    pub backoff_ms: u64,
    pub backoff_multiplier: f64,
    pub retry_on: Vec<String>, // HTTP status codes or error types
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservabilityConfig {
    pub metrics: MetricsConfig,
    pub tracing: TracingConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsConfig {
    pub enabled: bool,
    pub prometheus_port: u16,
    pub custom_metrics: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracingConfig {
    pub enabled: bool,
    pub jaeger_endpoint: Option<String>,
    pub sampling_rate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
    pub format: String, // "json", "text"
    pub output: String, // "stdout", "file"
    pub file_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiConfig {
    pub enabled: bool,
    pub provider: String, // "openai", "azure", "anthropic"
    pub api_key: Option<String>,
    pub endpoint: Option<String>,
    pub model: String,
    pub autonomous_mode: bool,
    pub decision_threshold: f64,
    pub learning_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    pub rate_limiting: RateLimitConfig,
    pub authentication: AuthConfig,
    pub authorization: AuthzConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    pub enabled: bool,
    pub requests_per_second: u32,
    pub burst_size: u32,
    pub key_extractor: String, // "ip", "header", "jwt_claim"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    pub enabled: bool,
    pub method: String, // "jwt", "oauth2", "mtls"
    pub config: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthzConfig {
    pub enabled: bool,
    pub policy_engine: String, // "opa", "casbin", "builtin"
    pub policies: Vec<String>,
}

impl ProxyConfig {
    pub fn load(path: &str) -> Result<Self> {
        let content = fs::read_to_string(path)?;
        let config: ProxyConfig = serde_yaml::from_str(&content)?;
        Ok(config)
    }
    
    pub fn generate_sample(output_path: &str) -> Result<()> {
        let sample_config = Self::default_config();
        let yaml = serde_yaml::to_string(&sample_config)?;
        fs::write(output_path, yaml)?;
        Ok(())
    }
    
    fn default_config() -> Self {
        let mut services = HashMap::new();
        services.insert("example-service".to_string(), ServiceConfig {
            endpoints: vec![
                EndpointConfig {
                    address: "127.0.0.1".to_string(),
                    port: 8080,
                    weight: 100,
                    metadata: HashMap::new(),
                }
            ],
            health_check: HealthCheckConfig {
                enabled: true,
                path: "/health".to_string(),
                interval_ms: 30000,
                timeout_ms: 5000,
                healthy_threshold: 2,
                unhealthy_threshold: 3,
            },
            timeout_ms: 30000,
        });
        
        ProxyConfig {
            server: ServerConfig {
                bind_address: "0.0.0.0".to_string(),
                port: 8000,
                tls: None,
                max_connections: 10000,
                connection_timeout_ms: 30000,
                request_timeout_ms: 30000,
            },
            upstream: UpstreamConfig {
                services,
                discovery: ServiceDiscoveryConfig {
                    provider: "static".to_string(),
                    config: HashMap::new(),
                },
            },
            load_balancing: LoadBalancingConfig {
                algorithm: "round_robin".to_string(),
                sticky_sessions: false,
                session_affinity_key: None,
            },
            circuit_breaker: CircuitBreakerConfig {
                enabled: true,
                failure_threshold: 5,
                recovery_timeout_ms: 60000,
                half_open_max_calls: 3,
            },
            retry: RetryConfig {
                enabled: true,
                max_attempts: 3,
                backoff_ms: 1000,
                backoff_multiplier: 2.0,
                retry_on: vec!["5xx".to_string(), "timeout".to_string()],
            },
            observability: ObservabilityConfig {
                metrics: MetricsConfig {
                    enabled: true,
                    prometheus_port: 9090,
                    custom_metrics: vec![],
                },
                tracing: TracingConfig {
                    enabled: true,
                    jaeger_endpoint: None,
                    sampling_rate: 0.1,
                },
                logging: LoggingConfig {
                    level: "info".to_string(),
                    format: "json".to_string(),
                    output: "stdout".to_string(),
                    file_path: None,
                },
            },
            ai: AiConfig {
                enabled: false,
                provider: "openai".to_string(),
                api_key: None,
                endpoint: None,
                model: "gpt-4".to_string(),
                autonomous_mode: false,
                decision_threshold: 0.8,
                learning_enabled: true,
            },
            security: SecurityConfig {
                rate_limiting: RateLimitConfig {
                    enabled: true,
                    requests_per_second: 1000,
                    burst_size: 2000,
                    key_extractor: "ip".to_string(),
                },
                authentication: AuthConfig {
                    enabled: false,
                    method: "jwt".to_string(),
                    config: HashMap::new(),
                },
                authorization: AuthzConfig {
                    enabled: false,
                    policy_engine: "builtin".to_string(),
                    policies: vec![],
                },
            },
        }
    }
}
