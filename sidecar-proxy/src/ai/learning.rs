use std::sync::Arc;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use anyhow::Result;
use parking_lot::RwLock;
use std::collections::VecDeque;

use crate::config::ProxyConfig;
use super::agent::RequestFeatures;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningModel {
    pub model_type: ModelType,
    pub parameters: Vec<f64>,
    pub accuracy: f64,
    pub training_samples: usize,
    pub last_updated: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ModelType {
    LinearRegression,
    LogisticRegression,
    DecisionTree,
    NeuralNetwork,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainingData {
    pub features: RequestFeatures,
    pub label: f64, // 0.0 for normal, 1.0 for anomaly
    pub weight: f64,
}

pub struct LearningEngine {
    config: Arc<ProxyConfig>,
    models: RwLock<Vec<LearningModel>>,
    training_buffer: RwLock<VecDeque<TrainingData>>,
    feature_stats: RwLock<FeatureStatistics>,
}

#[derive(Debug, Default)]
struct FeatureStatistics {
    response_time_mean: f64,
    response_time_std: f64,
    request_size_mean: f64,
    request_size_std: f64,
    response_size_mean: f64,
    response_size_std: f64,
    sample_count: usize,
}

impl LearningEngine {
    pub async fn new(config: Arc<ProxyConfig>) -> Result<Self> {
        let models = RwLock::new(vec![
            LearningModel {
                model_type: ModelType::LogisticRegression,
                parameters: vec![0.0; 10], // Initialize with zeros
                accuracy: 0.5,
                training_samples: 0,
                last_updated: chrono::Utc::now(),
            }
        ]);
        
        Ok(Self {
            config,
            models,
            training_buffer: RwLock::new(VecDeque::with_capacity(10000)),
            feature_stats: RwLock::new(FeatureStatistics::default()),
        })
    }
    
    pub async fn update_model(&self, features: &RequestFeatures) {
        if !self.config.ai.learning_enabled {
            return;
        }
        
        self.update_feature_stats(features).await;
        
        let is_anomaly = self.is_anomaly_heuristic(features).await;
        
        let training_data = TrainingData {
            features: features.clone(),
            label: if is_anomaly { 1.0 } else { 0.0 },
            weight: 1.0,
        };
        
        {
            let mut buffer = self.training_buffer.write();
            buffer.push_back(training_data);
            
            if buffer.len() > 10000 {
                buffer.pop_front();
            }
        }
        
        let buffer_size = self.training_buffer.read().len();
        if buffer_size % 1000 == 0 && buffer_size > 0 {
            self.retrain_models().await;
        }
    }
    
    pub async fn predict_anomaly(&self, features: &RequestFeatures) -> f64 {
        let models = self.models.read();
        if models.is_empty() {
            return 0.0;
        }
        
        let model = &models[0];
        let feature_vector = self.extract_feature_vector(features).await;
        
        match model.model_type {
            ModelType::LogisticRegression => {
                self.logistic_regression_predict(&model.parameters, &feature_vector)
            }
            ModelType::LinearRegression => {
                self.linear_regression_predict(&model.parameters, &feature_vector)
            }
            _ => 0.0, // Not implemented
        }
    }
    
    async fn update_feature_stats(&self, features: &RequestFeatures) {
        let mut stats = self.feature_stats.write();
        
        let n = stats.sample_count as f64;
        let response_time = features.response_time_ms as f64;
        let request_size = features.request_size as f64;
        let response_size = features.response_size as f64;
        
        stats.sample_count += 1;
        let new_n = stats.sample_count as f64;
        
        let delta_rt = response_time - stats.response_time_mean;
        stats.response_time_mean += delta_rt / new_n;
        let delta2_rt = response_time - stats.response_time_mean;
        stats.response_time_std = ((n * stats.response_time_std.powi(2) + delta_rt * delta2_rt) / new_n).sqrt();
        
        let delta_req = request_size - stats.request_size_mean;
        stats.request_size_mean += delta_req / new_n;
        let delta2_req = request_size - stats.request_size_mean;
        stats.request_size_std = ((n * stats.request_size_std.powi(2) + delta_req * delta2_req) / new_n).sqrt();
        
        let delta_resp = response_size - stats.response_size_mean;
        stats.response_size_mean += delta_resp / new_n;
        let delta2_resp = response_size - stats.response_size_mean;
        stats.response_size_std = ((n * stats.response_size_std.powi(2) + delta_resp * delta2_resp) / new_n).sqrt();
    }
    
    async fn is_anomaly_heuristic(&self, features: &RequestFeatures) -> bool {
        let stats = self.feature_stats.read();
        
        if stats.sample_count < 100 {
            return false; // Not enough data for reliable detection
        }
        
        let response_time = features.response_time_ms as f64;
        let request_size = features.request_size as f64;
        let response_size = features.response_size as f64;
        
        let rt_zscore = if stats.response_time_std > 0.0 {
            (response_time - stats.response_time_mean).abs() / stats.response_time_std
        } else {
            0.0
        };
        
        let req_zscore = if stats.request_size_std > 0.0 {
            (request_size - stats.request_size_mean).abs() / stats.request_size_std
        } else {
            0.0
        };
        
        let resp_zscore = if stats.response_size_std > 0.0 {
            (response_size - stats.response_size_mean).abs() / stats.response_size_std
        } else {
            0.0
        };
        
        rt_zscore > 3.0 || req_zscore > 3.0 || resp_zscore > 3.0 || features.status_code >= 500
    }
    
    async fn extract_feature_vector(&self, features: &RequestFeatures) -> Vec<f64> {
        let stats = self.feature_stats.read();
        
        let response_time_norm = if stats.response_time_std > 0.0 {
            (features.response_time_ms as f64 - stats.response_time_mean) / stats.response_time_std
        } else {
            0.0
        };
        
        let request_size_norm = if stats.request_size_std > 0.0 {
            (features.request_size as f64 - stats.request_size_mean) / stats.request_size_std
        } else {
            0.0
        };
        
        let response_size_norm = if stats.response_size_std > 0.0 {
            (features.response_size as f64 - stats.response_size_mean) / stats.response_size_std
        } else {
            0.0
        };
        
        vec![
            1.0, // Bias term
            response_time_norm,
            request_size_norm,
            response_size_norm,
            if features.status_code >= 400 { 1.0 } else { 0.0 }, // Error indicator
            if features.method == "GET" { 1.0 } else { 0.0 }, // Method indicators
            if features.method == "POST" { 1.0 } else { 0.0 },
            if features.method == "PUT" { 1.0 } else { 0.0 },
            if features.method == "DELETE" { 1.0 } else { 0.0 },
            features.path.len() as f64 / 100.0, // Path length (normalized)
        ]
    }
    
    fn logistic_regression_predict(&self, parameters: &[f64], features: &[f64]) -> f64 {
        let dot_product: f64 = parameters.iter()
            .zip(features.iter())
            .map(|(w, x)| w * x)
            .sum();
        
        1.0 / (1.0 + (-dot_product).exp())
    }
    
    fn linear_regression_predict(&self, parameters: &[f64], features: &[f64]) -> f64 {
        parameters.iter()
            .zip(features.iter())
            .map(|(w, x)| w * x)
            .sum()
    }
    
    async fn retrain_models(&self) {
        info!("Retraining machine learning models");
        
        let training_data: Vec<TrainingData> = {
            let buffer = self.training_buffer.read();
            buffer.iter().cloned().collect()
        };
        
        if training_data.len() < 100 {
            debug!("Not enough training data for retraining");
            return;
        }
        
        let mut models = self.models.write();
        if let Some(model) = models.get_mut(0) {
            if matches!(model.model_type, ModelType::LogisticRegression) {
                let new_parameters = self.train_logistic_regression(&training_data).await;
                let accuracy = self.evaluate_model(&new_parameters, &training_data).await;
                
                if accuracy > model.accuracy {
                    info!("Model improved: accuracy {:.3} -> {:.3}", model.accuracy, accuracy);
                    model.parameters = new_parameters;
                    model.accuracy = accuracy;
                    model.training_samples = training_data.len();
                    model.last_updated = chrono::Utc::now();
                } else {
                    debug!("Model did not improve, keeping existing parameters");
                }
            }
        }
    }
    
    async fn train_logistic_regression(&self, training_data: &[TrainingData]) -> Vec<f64> {
        let feature_dim = 10; // Based on extract_feature_vector
        let mut parameters = vec![0.0; feature_dim];
        let learning_rate = 0.01;
        let epochs = 100;
        
        for _epoch in 0..epochs {
            let mut gradients = vec![0.0; feature_dim];
            
            for data in training_data {
                let features = self.extract_feature_vector(&data.features).await;
                let prediction = self.logistic_regression_predict(&parameters, &features);
                let error = prediction - data.label;
                
                for (i, &feature) in features.iter().enumerate() {
                    gradients[i] += error * feature * data.weight;
                }
            }
            
            for (i, gradient) in gradients.iter().enumerate() {
                parameters[i] -= learning_rate * gradient / training_data.len() as f64;
            }
        }
        
        parameters
    }
    
    async fn evaluate_model(&self, parameters: &[f64], test_data: &[TrainingData]) -> f64 {
        let mut correct = 0;
        let mut total = 0;
        
        for data in test_data {
            let features = self.extract_feature_vector(&data.features).await;
            let prediction = self.logistic_regression_predict(parameters, &features);
            let predicted_class = if prediction > 0.5 { 1.0 } else { 0.0 };
            
            if (predicted_class - data.label).abs() < 0.1 {
                correct += 1;
            }
            total += 1;
        }
        
        if total > 0 {
            correct as f64 / total as f64
        } else {
            0.0
        }
    }
    
    pub fn get_model_info(&self) -> Vec<LearningModel> {
        self.models.read().clone()
    }
    
    pub fn get_training_stats(&self) -> (usize, FeatureStatistics) {
        let buffer_size = self.training_buffer.read().len();
        let stats = self.feature_stats.read().clone();
        (buffer_size, stats)
    }
}

impl Clone for FeatureStatistics {
    fn clone(&self) -> Self {
        Self {
            response_time_mean: self.response_time_mean,
            response_time_std: self.response_time_std,
            request_size_mean: self.request_size_mean,
            request_size_std: self.request_size_std,
            response_size_mean: self.response_size_mean,
            response_size_std: self.response_size_std,
            sample_count: self.sample_count,
        }
    }
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
    async fn test_learning_engine_creation() {
        let config = Arc::new(ProxyConfig::default_config());
        let engine = LearningEngine::new(config).await.unwrap();
        
        let models = engine.get_model_info();
        assert_eq!(models.len(), 1);
        assert!(matches!(models[0].model_type, ModelType::LogisticRegression));
    }
    
    #[tokio::test]
    async fn test_feature_extraction() {
        let config = Arc::new(ProxyConfig::default_config());
        let engine = LearningEngine::new(config).await.unwrap();
        
        let features = create_test_features();
        let feature_vector = engine.extract_feature_vector(&features).await;
        
        assert_eq!(feature_vector.len(), 10);
        assert_eq!(feature_vector[0], 1.0); // Bias term
    }
    
    #[tokio::test]
    async fn test_model_update() {
        let mut config = ProxyConfig::default_config();
        config.ai.learning_enabled = true;
        let config = Arc::new(config);
        let engine = LearningEngine::new(config).await.unwrap();
        
        let features = create_test_features();
        engine.update_model(&features).await;
        
        let (buffer_size, _) = engine.get_training_stats();
        assert_eq!(buffer_size, 1);
    }
}
