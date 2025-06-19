use std::sync::Arc;
use std::collections::HashMap;
use tokio::time::{interval, Duration};
use tracing::{info, warn, error};
use anyhow::Result;
use dashmap::DashMap;
use parking_lot::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::config::{ProxyConfig, ServiceConfig, EndpointConfig};

#[derive(Debug)]
pub struct Endpoint {
    pub address: String,
    pub port: u16,
    pub weight: u32,
    pub healthy: std::sync::atomic::AtomicBool,
    pub metadata: HashMap<String, String>,
    pub active_connections: AtomicUsize,
    pub total_requests: AtomicUsize,
    pub failed_requests: AtomicUsize,
    pub avg_response_time: RwLock<f64>,
}

pub struct LoadBalancer {
    config: Arc<ProxyConfig>,
    services: DashMap<String, ServiceEndpoints>,
    algorithm: LoadBalancingAlgorithm,
}

struct ServiceEndpoints {
    endpoints: Vec<Arc<Endpoint>>,
    round_robin_index: AtomicUsize,
    consistent_hash_ring: Option<ConsistentHashRing>,
}

#[derive(Debug, Clone)]
enum LoadBalancingAlgorithm {
    RoundRobin,
    LeastConnections,
    WeightedRoundRobin,
    ConsistentHash,
}

struct ConsistentHashRing {
    ring: std::collections::BTreeMap<u64, Arc<Endpoint>>,
    virtual_nodes: usize,
}

impl LoadBalancer {
    pub async fn new(config: Arc<ProxyConfig>) -> Result<Self> {
        let algorithm = match config.load_balancing.algorithm.as_str() {
            "round_robin" => LoadBalancingAlgorithm::RoundRobin,
            "least_connections" => LoadBalancingAlgorithm::LeastConnections,
            "weighted_round_robin" => LoadBalancingAlgorithm::WeightedRoundRobin,
            "consistent_hash" => LoadBalancingAlgorithm::ConsistentHash,
            _ => LoadBalancingAlgorithm::RoundRobin,
        };
        
        let services = DashMap::new();
        
        for (service_name, service_config) in &config.upstream.services {
            let endpoints: Vec<Arc<Endpoint>> = service_config
                .endpoints
                .iter()
                .map(|ep| Arc::new(Endpoint::from_config(ep)))
                .collect();
            
            let consistent_hash_ring = if matches!(algorithm, LoadBalancingAlgorithm::ConsistentHash) {
                Some(ConsistentHashRing::new(&endpoints, 150))
            } else {
                None
            };
            
            services.insert(service_name.clone(), ServiceEndpoints {
                endpoints,
                round_robin_index: AtomicUsize::new(0),
                consistent_hash_ring,
            });
        }
        
        Ok(Self {
            config,
            services,
            algorithm,
        })
    }
    
    pub async fn select_endpoint(&self, service_name: &str) -> Option<String> {
        let service_endpoints = self.services.get(service_name)?;
        
        let healthy_endpoints: Vec<_> = service_endpoints
            .endpoints
            .iter()
            .filter(|ep| ep.healthy.load(std::sync::atomic::Ordering::Relaxed))
            .collect();
        
        if healthy_endpoints.is_empty() {
            warn!("No healthy endpoints available for service: {}", service_name);
            return None;
        }
        
        let selected_endpoint = match &self.algorithm {
            LoadBalancingAlgorithm::RoundRobin => {
                self.select_round_robin(&healthy_endpoints, &service_endpoints.round_robin_index)
            }
            LoadBalancingAlgorithm::LeastConnections => {
                self.select_least_connections(&healthy_endpoints)
            }
            LoadBalancingAlgorithm::WeightedRoundRobin => {
                self.select_weighted_round_robin(&healthy_endpoints)
            }
            LoadBalancingAlgorithm::ConsistentHash => {
                if let Some(ring) = &service_endpoints.consistent_hash_ring {
                    self.select_consistent_hash(ring, "default_key")
                } else {
                    self.select_round_robin(&healthy_endpoints, &service_endpoints.round_robin_index)
                }
            }
        };
        
        selected_endpoint.map(|ep| format!("{}:{}", ep.address, ep.port))
    }
    
    fn select_round_robin(
        &self,
        endpoints: &[&Arc<Endpoint>],
        index: &AtomicUsize,
    ) -> Option<Arc<Endpoint>> {
        if endpoints.is_empty() {
            return None;
        }
        
        let idx = index.fetch_add(1, Ordering::Relaxed) % endpoints.len();
        Some(endpoints[idx].clone())
    }
    
    fn select_least_connections(&self, endpoints: &[&Arc<Endpoint>]) -> Option<Arc<Endpoint>> {
        endpoints
            .iter()
            .min_by_key(|ep| ep.active_connections.load(Ordering::Relaxed))
            .map(|ep| (*ep).clone())
    }
    
    fn select_weighted_round_robin(&self, endpoints: &[&Arc<Endpoint>]) -> Option<Arc<Endpoint>> {
        let total_weight: u32 = endpoints.iter().map(|ep| ep.weight).sum();
        if total_weight == 0 {
            return self.select_round_robin(endpoints, &AtomicUsize::new(0));
        }
        
        let mut random_weight = fastrand::u32(0..total_weight);
        
        for endpoint in endpoints {
            if random_weight < endpoint.weight {
                return Some((*endpoint).clone());
            }
            random_weight -= endpoint.weight;
        }
        
        endpoints.first().map(|ep| (*ep).clone())
    }
    
    fn select_consistent_hash(
        &self,
        ring: &ConsistentHashRing,
        key: &str,
    ) -> Option<Arc<Endpoint>> {
        ring.get_endpoint(key)
    }
    
    pub async fn start_health_checks(&self) {
        for service_entry in self.services.iter() {
            let service_name = service_entry.key().clone();
            let endpoints = service_entry.value().endpoints.clone();
            let config = self.config.clone();
            
            tokio::spawn(async move {
                let service_config = config.upstream.services.get(&service_name);
                if let Some(service_config) = service_config {
                    if service_config.health_check.enabled {
                        Self::health_check_loop(endpoints, service_config.clone()).await;
                    }
                }
            });
        }
    }
    
    async fn health_check_loop(endpoints: Vec<Arc<Endpoint>>, config: ServiceConfig) {
        let mut interval = interval(Duration::from_millis(config.health_check.interval_ms));
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(config.health_check.timeout_ms))
            .build()
            .unwrap();
        
        loop {
            interval.tick().await;
            
            for endpoint in &endpoints {
                let url = format!("http://{}:{}{}", 
                    endpoint.address, 
                    endpoint.port, 
                    config.health_check.path
                );
                
                let is_healthy = match client.get(&url).send().await {
                    Ok(response) => response.status().is_success(),
                    Err(err) => {
                        error!("Health check failed for {}: {}", url, err);
                        false
                    }
                };
                
                let was_healthy = endpoint.healthy.load(std::sync::atomic::Ordering::Relaxed);
                endpoint.set_healthy(is_healthy);
                
                if was_healthy != is_healthy {
                    if is_healthy {
                        info!("Endpoint {} is now healthy", url);
                    } else {
                        warn!("Endpoint {} is now unhealthy", url);
                    }
                }
            }
        }
    }
    
    pub fn update_endpoint_stats(&self, service_name: &str, endpoint_addr: &str, response_time: Duration, success: bool) {
        if let Some(service_endpoints) = self.services.get(service_name) {
            for endpoint in &service_endpoints.endpoints {
                let addr = format!("{}:{}", endpoint.address, endpoint.port);
                if addr == endpoint_addr {
                    endpoint.total_requests.fetch_add(1, Ordering::Relaxed);
                    if !success {
                        endpoint.failed_requests.fetch_add(1, Ordering::Relaxed);
                    }
                    
                    let mut avg_time = endpoint.avg_response_time.write();
                    let new_time = response_time.as_millis() as f64;
                    *avg_time = if *avg_time == 0.0 {
                        new_time
                    } else {
                        0.9 * *avg_time + 0.1 * new_time
                    };
                    break;
                }
            }
        }
    }
}

impl Endpoint {
    fn from_config(config: &EndpointConfig) -> Self {
        Self {
            address: config.address.clone(),
            port: config.port,
            weight: config.weight,
            healthy: std::sync::atomic::AtomicBool::new(true),
            metadata: config.metadata.clone(),
            active_connections: AtomicUsize::new(0),
            total_requests: AtomicUsize::new(0),
            failed_requests: AtomicUsize::new(0),
            avg_response_time: RwLock::new(0.0),
        }
    }
    
    fn set_healthy(&self, healthy: bool) {
        self.healthy.store(healthy, std::sync::atomic::Ordering::Relaxed);
    }
}

impl ConsistentHashRing {
    fn new(endpoints: &[Arc<Endpoint>], virtual_nodes: usize) -> Self {
        let mut ring = std::collections::BTreeMap::new();
        
        for endpoint in endpoints {
            for i in 0..virtual_nodes {
                let key = format!("{}:{}:{}", endpoint.address, endpoint.port, i);
                let hash = Self::hash(&key);
                ring.insert(hash, endpoint.clone());
            }
        }
        
        Self {
            ring,
            virtual_nodes,
        }
    }
    
    fn get_endpoint(&self, key: &str) -> Option<Arc<Endpoint>> {
        if self.ring.is_empty() {
            return None;
        }
        
        let hash = Self::hash(key);
        
        if let Some((_, endpoint)) = self.ring.range(hash..).next() {
            Some(endpoint.clone())
        } else {
            self.ring.values().next().cloned()
        }
    }
    
    fn hash(key: &str) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        hasher.finish()
    }
}
