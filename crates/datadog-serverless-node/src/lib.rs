// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

use datadog_serverless_core::config::ServicesConfig;
use datadog_serverless_core::error::ServicesError;
use datadog_serverless_core::{ServerlessServices, ServicesHandle};
use napi::Error;
use napi_derive::napi;
use std::sync::{Arc, Mutex};

#[napi]
pub fn hello() -> String {
    "Hello from Datadog Serverless Node!".to_string()
}

/// JavaScript-compatible configuration object
#[napi(object)]
#[derive(Debug, Clone)]
pub struct JsServicesConfig {
    /// Datadog API key for authentication
    pub api_key: Option<String>,
    /// DogStatsD server port (default: 8125)
    pub dogstatsd_port: Option<u32>,
    /// Datadog site (default: datadoghq.com)
    pub site: Option<String>,
    /// Whether to enable DogStatsD (default: true)
    pub use_dogstatsd: Option<bool>,
    /// Optional metric namespace prefix
    pub metric_namespace: Option<String>,
    /// HTTPS proxy URL
    pub https_proxy: Option<String>,
    /// Log level: trace, debug, info, warn, error (default: info)
    pub log_level: Option<String>,
}

impl JsServicesConfig {
    /// Convert JavaScript config to Rust config with validation
    pub fn into_rust_config(self) -> Result<ServicesConfig, ServicesError> {
        let config = ServicesConfig {
            api_key: self.api_key,
            dogstatsd_port: self.dogstatsd_port.map(|p| p as u16).unwrap_or(8125),
            site: self.site.unwrap_or_else(|| "datadoghq.com".to_string()),
            use_dogstatsd: self.use_dogstatsd.unwrap_or(true),
            metric_namespace: self.metric_namespace,
            https_proxy: self.https_proxy,
            log_level: self
                .log_level
                .unwrap_or_else(|| "info".to_string())
                .to_lowercase(),
        };

        config.validate()?;
        Ok(config)
    }
}

/// Main class for controlling Datadog serverless services from Node.js
#[napi]
pub struct DatadogServices {
    handle: Arc<Mutex<Option<ServicesHandle>>>,
    runtime: Arc<tokio::runtime::Runtime>,
}

#[napi]
impl DatadogServices {
    /// Create a new DatadogServices instance
    #[napi(constructor)]
    pub fn new() -> napi::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::from_reason(format!("Failed to create runtime: {}", e)))?;

        Ok(Self {
            handle: Arc::new(Mutex::new(None)),
            runtime: Arc::new(runtime),
        })
    }

    /// Start the Datadog services
    #[napi]
    pub async fn start(&self, config: JsServicesConfig) -> napi::Result<()> {
        // Check if already running before starting
        {
            let guard = self
                .handle
                .lock()
                .map_err(|e| Error::from_reason(format!("Lock error: {}", e)))?;

            if guard.is_some() {
                return Err(Error::from_reason("Services already started"));
            }
        } // Drop guard before async operations

        let rust_config = config
            .into_rust_config()
            .map_err(|e| Error::from_reason(format!("Invalid configuration: {}", e)))?;

        // Create services
        let services = ServerlessServices::new(rust_config);

        // Start services
        let handle = services
            .start()
            .await
            .map_err(|e| Error::from_reason(format!("Failed to start services: {}", e)))?;

        // Store handle
        let mut guard = self
            .handle
            .lock()
            .map_err(|e| Error::from_reason(format!("Lock error: {}", e)))?;

        *guard = Some(handle);

        Ok(())
    }

    /// Stop the Datadog services
    #[napi]
    pub async fn stop(&self) -> napi::Result<()> {
        // Take handle before async operations
        let handle = {
            let mut guard = self
                .handle
                .lock()
                .map_err(|e| Error::from_reason(format!("Lock error: {}", e)))?;

            guard
                .take()
                .ok_or_else(|| Error::from_reason("Services not running"))?
        }; // Drop guard before async operations

        handle
            .stop()
            .await
            .map_err(|e| Error::from_reason(format!("Failed to stop services: {}", e)))?;

        Ok(())
    }

    /// Check if services are running
    #[napi]
    pub fn is_running(&self) -> napi::Result<bool> {
        let guard = self
            .handle
            .lock()
            .map_err(|e| Error::from_reason(format!("Lock error: {}", e)))?;

        Ok(guard.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_js_config_conversion() {
        let js_config = JsServicesConfig {
            api_key: Some("test_key".to_string()),
            dogstatsd_port: Some(8126),
            site: Some("datadoghq.eu".to_string()),
            use_dogstatsd: Some(true),
            metric_namespace: Some("test".to_string()),
            https_proxy: Some("https://proxy.example.com".to_string()),
            log_level: Some("debug".to_string()),
        };

        let rust_config = js_config.into_rust_config().unwrap();
        assert_eq!(rust_config.api_key, Some("test_key".to_string()));
        assert_eq!(rust_config.dogstatsd_port, 8126);
        assert_eq!(rust_config.site, "datadoghq.eu");
        assert_eq!(rust_config.use_dogstatsd, true);
        assert_eq!(rust_config.metric_namespace, Some("test".to_string()));
        assert_eq!(
            rust_config.https_proxy,
            Some("https://proxy.example.com".to_string())
        );
        assert_eq!(rust_config.log_level, "debug");
    }

    #[test]
    fn test_js_config_defaults() {
        let js_config = JsServicesConfig {
            api_key: None,
            dogstatsd_port: None,
            site: None,
            use_dogstatsd: None,
            metric_namespace: None,
            https_proxy: None,
            log_level: None,
        };

        let rust_config = js_config.into_rust_config().unwrap();
        assert_eq!(rust_config.api_key, None);
        assert_eq!(rust_config.dogstatsd_port, 8125);
        assert_eq!(rust_config.site, "datadoghq.com");
        assert_eq!(rust_config.use_dogstatsd, true);
        assert_eq!(rust_config.metric_namespace, None);
        assert_eq!(rust_config.https_proxy, None);
        assert_eq!(rust_config.log_level, "info");
    }

    #[test]
    fn test_js_config_validation() {
        // Test invalid port
        let js_config = JsServicesConfig {
            api_key: None,
            dogstatsd_port: Some(0),
            site: None,
            use_dogstatsd: None,
            metric_namespace: None,
            https_proxy: None,
            log_level: None,
        };
        assert!(js_config.into_rust_config().is_err());

        // Test invalid log level
        let js_config = JsServicesConfig {
            api_key: None,
            dogstatsd_port: None,
            site: None,
            use_dogstatsd: None,
            metric_namespace: None,
            https_proxy: None,
            log_level: Some("invalid".to_string()),
        };
        assert!(js_config.into_rust_config().is_err());

        // Test empty site
        let js_config = JsServicesConfig {
            api_key: None,
            dogstatsd_port: None,
            site: Some("".to_string()),
            use_dogstatsd: None,
            metric_namespace: None,
            https_proxy: None,
            log_level: None,
        };
        assert!(js_config.into_rust_config().is_err());
    }
}
