//! Tag provider for the Datadog Agent
//!
//! This module provides a general-purpose tag provider that combines tags from
//! configuration and environment variables.

use crate::config;
use std::collections::hash_map::HashMap;
use std::sync::Arc;

/// Tag provider for the Datadog Agent
#[derive(Debug, Clone)]
pub struct Provider {
    tags: HashMap<String, String>,
    service: Option<String>,
    #[allow(dead_code)]
    env: Option<String>,
    #[allow(dead_code)]
    version: Option<String>,
}

impl Provider {
    /// Create a new tag provider from configuration
    #[must_use]
    pub fn new(config: Arc<config::Config>) -> Self {
        let mut tags = config.tags.clone();

        // Decision: Add standard observability tags (service, env, version) if configured
        // These are Datadog's unified service tagging convention for consistent filtering
        if let Some(ref service) = config.service {
            tags.insert("service".to_string(), service.clone());
        }
        if let Some(ref env) = config.env {
            tags.insert("env".to_string(), env.clone());
        }
        if let Some(ref version) = config.version {
            tags.insert("version".to_string(), version.clone());
        }

        Provider {
            tags,
            service: config.service.clone(),
            env: config.env.clone(),
            version: config.version.clone(),
        }
    }

    /// Get tags as a vector of "key:value" strings
    #[must_use]
    pub fn get_tags_vec(&self) -> Vec<String> {
        self.tags.iter().map(|(k, v)| format!("{k}:{v}")).collect()
    }

    /// Get tags as a comma-separated string
    #[must_use]
    pub fn get_tags_string(&self) -> String {
        self.get_tags_vec().join(",")
    }

    /// Get canonical ID (for general agent, this is None)
    #[must_use]
    pub fn get_canonical_id(&self) -> Option<String> {
        None
    }

    /// Get canonical resource name (service name if available)
    #[must_use]
    pub fn get_canonical_resource_name(&self) -> Option<String> {
        self.service.clone()
    }

    /// Get tags as a HashMap
    #[must_use]
    pub fn get_tags_map(&self) -> &HashMap<String, String> {
        &self.tags
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn test_provider_new() {
        let config = Arc::new(Config {
            service: Some("test-service".to_string()),
            env: Some("test".to_string()),
            version: Some("1.0.0".to_string()),
            tags: HashMap::from([
                ("team".to_string(), "platform".to_string()),
                ("app".to_string(), "my-app".to_string()),
            ]),
            ..Config::default()
        });

        let provider = Provider::new(config);
        let tags_str = provider.get_tags_string();

        assert!(tags_str.contains("service:test-service"));
        assert!(tags_str.contains("env:test"));
        assert!(tags_str.contains("version:1.0.0"));
        assert!(tags_str.contains("team:platform"));
        assert!(tags_str.contains("app:my-app"));
    }

    #[test]
    fn test_provider_canonical_resource_name() {
        let config = Arc::new(Config {
            service: Some("my-service".to_string()),
            ..Config::default()
        });

        let provider = Provider::new(config);
        assert_eq!(
            provider.get_canonical_resource_name(),
            Some("my-service".to_string())
        );
    }

    #[test]
    fn test_provider_no_service() {
        let config = Arc::new(Config {
            tags: HashMap::from([("env".to_string(), "prod".to_string())]),
            ..Config::default()
        });

        let provider = Provider::new(config);
        assert_eq!(provider.get_canonical_resource_name(), None);
        assert!(provider.get_tags_string().contains("env:prod"));
    }
}
