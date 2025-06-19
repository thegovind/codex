use std::sync::Arc;
use std::time::Duration;
use futures::Future;
use tracing::{debug, warn};
use anyhow::Result;

use crate::config::ProxyConfig;

pub struct RetryPolicy {
    config: Arc<ProxyConfig>,
}

#[derive(Debug, Clone)]
pub struct RetryableError {
    pub message: String,
    pub retryable: bool,
    pub status_code: Option<u16>,
}

impl RetryPolicy {
    pub fn new(config: Arc<ProxyConfig>) -> Self {
        Self { config }
    }
    
    pub async fn execute<F, Fut, T, E>(&self, mut operation: F) -> Result<T, E>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: std::fmt::Display + Clone,
    {
        if !self.config.retry.enabled {
            return operation().await;
        }
        
        let max_attempts = self.config.retry.max_attempts;
        let mut backoff_ms = self.config.retry.backoff_ms;
        let backoff_multiplier = self.config.retry.backoff_multiplier;
        
        for attempt in 1..=max_attempts {
            match operation().await {
                Ok(result) => {
                    if attempt > 1 {
                        debug!("Operation succeeded on attempt {}/{}", attempt, max_attempts);
                    }
                    return Ok(result);
                }
                Err(error) => {
                    if attempt == max_attempts {
                        warn!("Operation failed after {} attempts: {}", max_attempts, error);
                        return Err(error);
                    }
                    
                    if !self.should_retry(&error) {
                        warn!("Operation failed with non-retryable error: {}", error);
                        return Err(error);
                    }
                    
                    debug!("Operation failed on attempt {}/{}, retrying in {}ms: {}", 
                           attempt, max_attempts, backoff_ms, error);
                    
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms as f64 * backoff_multiplier) as u64;
                }
            }
        }
        
        unreachable!("Loop should have returned or broken before this point");
    }
    
    pub async fn execute_with_jitter<F, Fut, T, E>(&self, mut operation: F) -> Result<T, E>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: std::fmt::Display + Clone,
    {
        if !self.config.retry.enabled {
            return operation().await;
        }
        
        let max_attempts = self.config.retry.max_attempts;
        let mut base_backoff_ms = self.config.retry.backoff_ms;
        let backoff_multiplier = self.config.retry.backoff_multiplier;
        
        for attempt in 1..=max_attempts {
            match operation().await {
                Ok(result) => {
                    if attempt > 1 {
                        debug!("Operation succeeded on attempt {}/{}", attempt, max_attempts);
                    }
                    return Ok(result);
                }
                Err(error) => {
                    if attempt == max_attempts {
                        warn!("Operation failed after {} attempts: {}", max_attempts, error);
                        return Err(error);
                    }
                    
                    if !self.should_retry(&error) {
                        warn!("Operation failed with non-retryable error: {}", error);
                        return Err(error);
                    }
                    
                    let jitter = fastrand::u64(0..base_backoff_ms / 2);
                    let backoff_with_jitter = base_backoff_ms + jitter;
                    
                    debug!("Operation failed on attempt {}/{}, retrying in {}ms: {}", 
                           attempt, max_attempts, backoff_with_jitter, error);
                    
                    tokio::time::sleep(Duration::from_millis(backoff_with_jitter)).await;
                    base_backoff_ms = (base_backoff_ms as f64 * backoff_multiplier) as u64;
                }
            }
        }
        
        unreachable!("Loop should have returned or broken before this point");
    }
    
    fn should_retry<E: std::fmt::Display>(&self, error: &E) -> bool {
        let error_str = error.to_string().to_lowercase();
        
        for retry_condition in &self.config.retry.retry_on {
            match retry_condition.as_str() {
                "5xx" => {
                    if error_str.contains("500") || error_str.contains("502") || 
                       error_str.contains("503") || error_str.contains("504") {
                        return true;
                    }
                }
                "timeout" => {
                    if error_str.contains("timeout") || error_str.contains("timed out") {
                        return true;
                    }
                }
                "connection_error" => {
                    if error_str.contains("connection") || error_str.contains("network") {
                        return true;
                    }
                }
                "dns_error" => {
                    if error_str.contains("dns") || error_str.contains("name resolution") {
                        return true;
                    }
                }
                specific_code => {
                    if error_str.contains(specific_code) {
                        return true;
                    }
                }
            }
        }
        
        false
    }
    
    pub fn create_retryable_error(message: String, retryable: bool, status_code: Option<u16>) -> RetryableError {
        RetryableError {
            message,
            retryable,
            status_code,
        }
    }
}

impl std::fmt::Display for RetryableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RetryableError {}

#[derive(Debug, Clone)]
pub struct ExponentialBackoff {
    base_delay: Duration,
    max_delay: Duration,
    multiplier: f64,
    jitter: bool,
}

impl ExponentialBackoff {
    pub fn new(base_delay: Duration, max_delay: Duration, multiplier: f64, jitter: bool) -> Self {
        Self {
            base_delay,
            max_delay,
            multiplier,
            jitter,
        }
    }
    
    pub fn delay(&self, attempt: u32) -> Duration {
        let delay_ms = self.base_delay.as_millis() as f64 * self.multiplier.powi(attempt as i32 - 1);
        let delay_ms = delay_ms.min(self.max_delay.as_millis() as f64);
        
        let final_delay = if self.jitter {
            let jitter_range = delay_ms * 0.1; // 10% jitter
            let jitter = fastrand::f64() * jitter_range * 2.0 - jitter_range;
            (delay_ms + jitter).max(0.0)
        } else {
            delay_ms
        };
        
        Duration::from_millis(final_delay as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RetryConfig, ProxyConfig};
    
    fn create_test_config() -> Arc<ProxyConfig> {
        let mut config = ProxyConfig::default_config();
        config.retry = RetryConfig {
            enabled: true,
            max_attempts: 3,
            backoff_ms: 100,
            backoff_multiplier: 2.0,
            retry_on: vec!["5xx".to_string(), "timeout".to_string()],
        };
        Arc::new(config)
    }
    
    #[tokio::test]
    async fn test_retry_success_on_first_attempt() {
        let config = create_test_config();
        let retry_policy = RetryPolicy::new(config);
        
        let mut call_count = 0;
        let result = retry_policy.execute(|| {
            call_count += 1;
            async { Ok::<i32, String>(42) }
        }).await;
        
        assert_eq!(result.unwrap(), 42);
        assert_eq!(call_count, 1);
    }
    
    #[tokio::test]
    async fn test_retry_success_after_failures() {
        let config = create_test_config();
        let retry_policy = RetryPolicy::new(config);
        
        let mut call_count = 0;
        let result = retry_policy.execute(|| {
            call_count += 1;
            async move {
                if call_count < 3 {
                    Err("500 Internal Server Error".to_string())
                } else {
                    Ok(42)
                }
            }
        }).await;
        
        assert_eq!(result.unwrap(), 42);
        assert_eq!(call_count, 3);
    }
    
    #[tokio::test]
    async fn test_retry_exhausted() {
        let config = create_test_config();
        let retry_policy = RetryPolicy::new(config);
        
        let mut call_count = 0;
        let result = retry_policy.execute(|| {
            call_count += 1;
            async { Err::<i32, String>("500 Internal Server Error".to_string()) }
        }).await;
        
        assert!(result.is_err());
        assert_eq!(call_count, 3); // max_attempts
    }
    
    #[tokio::test]
    async fn test_non_retryable_error() {
        let config = create_test_config();
        let retry_policy = RetryPolicy::new(config);
        
        let mut call_count = 0;
        let result = retry_policy.execute(|| {
            call_count += 1;
            async { Err::<i32, String>("400 Bad Request".to_string()) }
        }).await;
        
        assert!(result.is_err());
        assert_eq!(call_count, 1); // Should not retry 4xx errors
    }
    
    #[test]
    fn test_exponential_backoff() {
        let backoff = ExponentialBackoff::new(
            Duration::from_millis(100),
            Duration::from_secs(10),
            2.0,
            false,
        );
        
        assert_eq!(backoff.delay(1), Duration::from_millis(100));
        assert_eq!(backoff.delay(2), Duration::from_millis(200));
        assert_eq!(backoff.delay(3), Duration::from_millis(400));
    }
}
