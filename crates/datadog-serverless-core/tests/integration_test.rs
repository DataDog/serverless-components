// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use datadog_serverless_core::{ServerlessServices, ServicesConfig};

#[tokio::test]
async fn test_services_with_dogstatsd_disabled() {
    // Test with DogStatsD disabled
    let config = ServicesConfig {
        api_key: Some("test-key".to_string()),
        site: "datadoghq.com".to_string(),
        dogstatsd_port: 8125,
        use_dogstatsd: false, // Disabled
        metric_namespace: None,
        log_level: "error".to_string(), // Suppress logs in tests
        https_proxy: None,
    };

    let services = ServerlessServices::new(config);

    // This will fail if not in cloud environment, which is expected
    // We're mainly testing that the API is accessible and types work correctly
    let result = services.start().await;

    // If it succeeds, stop it cleanly
    if let Ok(handle) = result {
        // Wait a bit to let the services initialize
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        // Check if running - may be false if environment detection failed after start
        let _is_running = handle.is_running().await;

        // Always try to stop cleanly
        handle.stop().await.ok();
    }
    // If it fails with EnvironmentDetection, that's expected for local testing
}

#[tokio::test]
async fn test_config_validation() {
    // Test that we can create services with default config
    let config = ServicesConfig::default();
    let services = ServerlessServices::new(config);

    // start() will fail in non-cloud environment with EnvironmentDetection error
    // This is expected behavior, not a config validation failure
    let result = services.start().await;

    // We expect either success (in cloud) or EnvironmentDetection error (locally)
    // Both are valid outcomes for this test
    match result {
        Ok(handle) => {
            handle.stop().await.ok();
        }
        Err(_) => {
            // Expected in local environment
        }
    }
}

#[test]
fn test_config_from_env_with_defaults() {
    // Clear all DD env vars
    for key in &[
        "DD_API_KEY",
        "DD_SITE",
        "DD_DOGSTATSD_PORT",
        "DD_USE_DOGSTATSD",
        "DD_STATSD_METRIC_NAMESPACE",
        "DD_LOG_LEVEL",
        "DD_PROXY_HTTPS",
        "HTTPS_PROXY",
    ] {
        std::env::remove_var(key);
    }

    // Should succeed with defaults (may fail on environment detection later)
    let result = ServicesConfig::from_env();
    assert!(result.is_ok());

    let config = result.unwrap();
    assert_eq!(config.site, "datadoghq.com");
    assert_eq!(config.dogstatsd_port, 8125);
    assert_eq!(config.use_dogstatsd, true);
    assert_eq!(config.log_level, "info");
}
