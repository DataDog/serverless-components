// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

/// Errors that can occur when inserting log entries into the aggregator.
#[derive(Debug, thiserror::Error)]
pub enum AggregatorError {
    #[error("log entry too large: {size} bytes exceeds max {max} bytes")]
    EntryTooLarge { size: usize, max: usize },

    #[error("failed to serialize log entry: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Errors that can occur when flushing logs to Datadog.
#[derive(Debug, thiserror::Error)]
pub enum FlushError {
    #[error("HTTP request failed: {0}")]
    Request(String),

    #[error("server returned permanent error: status {status}")]
    PermanentError { status: u16 },

    #[error("max retries exceeded after {attempts} attempts")]
    MaxRetriesExceeded { attempts: u32 },

    #[error("compression failed: {0}")]
    Compression(String),
}

/// Errors that can occur during crate object creation.
#[derive(Debug, thiserror::Error)]
pub enum CreationError {
    #[error("failed to build HTTP client: {0}")]
    HttpClient(String),
}
