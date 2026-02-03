// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

/// Errors that can occur when working with serverless services
#[derive(Debug, thiserror::Error)]
pub enum ServicesError {
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("Failed to detect cloud environment")]
    EnvironmentDetection,

    #[error("Failed to start trace agent: {0}")]
    TraceAgentStart(String),

    #[error("Failed to start DogStatsD: {0}")]
    DogStatsDStart(String),

    #[error("Services already started")]
    AlreadyStarted,

    #[error("Services not running")]
    NotRunning,

    #[error("Shutdown timeout exceeded")]
    ShutdownTimeout,

    #[error("Runtime error: {0}")]
    Runtime(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let error = ServicesError::InvalidConfig("missing API key".to_string());
        assert_eq!(error.to_string(), "Invalid configuration: missing API key");
    }

    #[test]
    fn test_error_debug() {
        let error = ServicesError::EnvironmentDetection;
        let debug_str = format!("{:?}", error);
        assert!(debug_str.contains("EnvironmentDetection"));
    }

    #[test]
    fn test_all_error_variants() {
        // Ensure all variants can be constructed
        let _e1 = ServicesError::InvalidConfig("test".into());
        let _e2 = ServicesError::EnvironmentDetection;
        let _e3 = ServicesError::TraceAgentStart("test".into());
        let _e4 = ServicesError::DogStatsDStart("test".into());
        let _e5 = ServicesError::AlreadyStarted;
        let _e6 = ServicesError::NotRunning;
        let _e7 = ServicesError::ShutdownTimeout;
        let _e8 = ServicesError::Runtime("test".into());
    }
}
