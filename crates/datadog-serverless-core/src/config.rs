// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use crate::error::ServicesError;
use dogstatsd::util::parse_metric_namespace;
use std::env;

/// Configuration for serverless services (trace agent and DogStatsD)
#[derive(Debug, Clone)]
pub struct ServicesConfig {
    /// Datadog API key for authentication
    pub api_key: Option<String>,
    /// DogStatsD server port
    pub dogstatsd_port: u16,
    /// Datadog site (e.g., datadoghq.com, datadoghq.eu)
    pub site: String,
    /// Whether to enable DogStatsD
    pub use_dogstatsd: bool,
    /// Optional metric namespace prefix
    pub metric_namespace: Option<String>,
    /// HTTPS proxy URL
    pub https_proxy: Option<String>,
    /// Log level (e.g., trace, debug, info, warn, error)
    pub log_level: String,
}

impl Default for ServicesConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            dogstatsd_port: 8125,
            site: "datadoghq.com".to_string(),
            use_dogstatsd: true,
            metric_namespace: None,
            https_proxy: None,
            log_level: "info".to_string(),
        }
    }
}

impl ServicesConfig {
    /// Create configuration from environment variables
    pub fn from_env() -> Result<Self, ServicesError> {
        let api_key = env::var("DD_API_KEY").ok();
        let dogstatsd_port = env::var("DD_DOGSTATSD_PORT")
            .ok()
            .and_then(|port| port.parse::<u16>().ok())
            .unwrap_or(8125);
        let site = env::var("DD_SITE").unwrap_or_else(|_| "datadoghq.com".to_string());
        let use_dogstatsd = env::var("DD_USE_DOGSTATSD")
            .map(|val| val.to_lowercase() != "false")
            .unwrap_or(true);
        let metric_namespace = env::var("DD_STATSD_METRIC_NAMESPACE")
            .ok()
            .and_then(|val| parse_metric_namespace(&val));
        let https_proxy = env::var("DD_PROXY_HTTPS")
            .or_else(|_| env::var("HTTPS_PROXY"))
            .ok();
        let log_level = env::var("DD_LOG_LEVEL")
            .map(|val| val.to_lowercase())
            .unwrap_or_else(|_| "info".to_string());

        let config = Self {
            api_key,
            dogstatsd_port,
            site,
            use_dogstatsd,
            metric_namespace,
            https_proxy,
            log_level,
        };

        config.validate()?;
        Ok(config)
    }

    /// Validate the configuration
    pub fn validate(&self) -> Result<(), ServicesError> {
        // Validate port range
        if self.dogstatsd_port == 0 {
            return Err(ServicesError::InvalidConfig(
                "DogStatsD port must be greater than 0".to_string(),
            ));
        }

        // Validate site is not empty
        if self.site.trim().is_empty() {
            return Err(ServicesError::InvalidConfig(
                "DD_SITE cannot be empty".to_string(),
            ));
        }

        // Validate log level
        let valid_log_levels = ["trace", "debug", "info", "warn", "error"];
        if !valid_log_levels.contains(&self.log_level.as_str()) {
            return Err(ServicesError::InvalidConfig(format!(
                "Invalid log level '{}'. Must be one of: trace, debug, info, warn, error",
                self.log_level
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_is_valid() {
        let config = ServicesConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_invalid_port() {
        let config = ServicesConfig {
            dogstatsd_port: 0,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_invalid_log_level() {
        let config = ServicesConfig {
            log_level: "invalid".to_string(),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_empty_site() {
        let config = ServicesConfig {
            site: "".to_string(),
            ..Default::default()
        };
        assert!(config.validate().is_err());

        let config = ServicesConfig {
            site: "   ".to_string(),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_valid_log_levels() {
        let valid_levels = ["trace", "debug", "info", "warn", "error"];
        for level in valid_levels {
            let config = ServicesConfig {
                log_level: level.to_string(),
                ..Default::default()
            };
            assert!(
                config.validate().is_ok(),
                "Log level '{}' should be valid",
                level
            );
        }
    }
}
