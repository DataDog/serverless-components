// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Trace statistics processing for APM metrics.
//!
//! This module handles the ingestion and processing of client-computed trace statistics,
//! which power APM metrics dashboards in Datadog without requiring full trace retention.
//!
//! # Statistics vs Traces
//!
//! - **Traces**: Full span details for individual requests (high cardinality, sampled)
//! - **Statistics**: Aggregated metrics about spans (low cardinality, always collected)
//!
//! Statistics provide metrics like:
//! - Request counts and error rates
//! - Latency percentiles (p50, p95, p99)
//! - Hit counts per resource
//! - Top errors by type
//!
//! # Processing Flow
//!
//! 1. **HTTP Request**: Client sends msgpack-encoded `ClientStatsPayload`
//! 2. **Validation**: Check content-length against `MAX_CONTENT_LENGTH` (50MB)
//! 3. **Deserialization**: Parse msgpack to protobuf `ClientStatsPayload` struct
//! 4. **Timestamping**: Set `start` timestamp to current Unix epoch (nanoseconds)
//! 5. **Buffering**: Send to aggregator via async mpsc channel
//! 6. **Response**: Return HTTP 202 Accepted
//!
//! # Why Client-Computed Stats?
//!
//! Client-side computation reduces agent overhead:
//! - Tracers aggregate stats locally before sending
//! - Agent receives pre-aggregated data (not per-span stats)
//! - Lower network bandwidth and agent CPU usage
//!
//! # Example Request
//!
//! ```bash
//! POST /v0.6/stats HTTP/1.1
//! Content-Type: application/msgpack
//! Content-Length: 1234
//!
//! <msgpack-encoded ClientStatsPayload>
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::{
    extract::Request,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use tokio::sync::mpsc::Sender;
use tracing::{debug, error};

use datadog_trace_protobuf::pb;
use datadog_trace_utils::stats_utils;
use ddcommon::hyper_migration;

use super::trace_agent::MAX_CONTENT_LENGTH;
use crate::http::extract_request_body;

/// Trait for processing client-computed trace statistics.
///
/// The `StatsProcessor` trait defines the interface for receiving and processing
/// trace statistics from APM clients. Statistics are pre-aggregated on the client
/// side and sent periodically to the agent.
///
/// # Responsibilities
///
/// - Validate request size and format
/// - Deserialize msgpack-encoded statistics payloads
/// - Set timestamps for aggregation bucketing
/// - Forward stats to the aggregator via async channel
#[async_trait]
pub trait StatsProcessor {
    /// Processes a client stats request and forwards stats to the aggregator.
    ///
    /// This method:
    /// 1. Extracts and validates the request body (max 50MB)
    /// 2. Deserializes msgpack to `ClientStatsPayload` protobuf struct
    /// 3. Sets the timestamp for aggregation bucketing
    /// 4. Sends stats to aggregator via the provided channel
    ///
    /// # Arguments
    ///
    /// * `req` - HTTP request containing msgpack-encoded stats
    /// * `tx` - Async channel sender to forward stats to aggregator
    ///
    /// # Returns
    ///
    /// - **200 Accepted**: Stats successfully buffered for aggregation
    /// - **400 Bad Request**: Invalid request body format
    /// - **413 Payload Too Large**: Content exceeds `MAX_CONTENT_LENGTH`
    /// - **500 Internal Server Error**: Deserialization or channel errors
    ///
    /// # Request Format
    ///
    /// The request body must be msgpack-encoded `ClientStatsPayload` with:
    /// - `stats`: Array of stat buckets (typically 1 bucket per flush)
    /// - `hostname`, `env`, `service`: Identification tags
    /// - `lang`, `tracer_version`, `runtime_id`: Tracer metadata
    ///
    /// # Example
    ///
    /// ```rust
    /// use axum::extract::Request;
    /// use tokio::sync::mpsc;
    /// use datadog_agent_native::traces::stats_processor::{StatsProcessor, ServerlessStatsProcessor};
    ///
    /// async fn handle_stats_request(req: Request) {
    ///     let processor = ServerlessStatsProcessor {};
    ///     let (tx, _rx) = mpsc::channel(100);
    ///
    ///     let response = processor.process_stats(req, tx).await.unwrap();
    ///     // Forward response to client...
    /// }
    /// ```
    async fn process_stats(
        &self,
        req: Request,
        tx: Sender<pb::ClientStatsPayload>,
    ) -> Result<Response, Box<dyn std::error::Error + Send + Sync>>;
}

/// Serverless-optimized stats processor for AWS Lambda and other serverless environments.
///
/// This processor is designed for serverless environments where:
/// - Fast processing is critical to minimize cold start overhead
/// - Stats are buffered in-memory for aggregation
/// - Flush happens before function termination
///
/// # Implementation Notes
///
/// - This is a zero-sized type (unit struct) as it holds no state
/// - Implements `Copy` for cheap cloning across async tasks
/// - All stats are buffered via async mpsc channel to the aggregator
///
/// # Buffering Strategy
///
/// Stats are not immediately aggregated - they are sent to an aggregator task:
/// 1. Stats arrive via HTTP request
/// 2. Processor validates and timestamps the stats
/// 3. Stats are sent through an async channel
/// 4. Aggregator task consumes stats and builds buckets
/// 5. Flusher sends aggregated buckets to Datadog
///
/// This allows the HTTP handler to respond quickly (202 Accepted) without
/// blocking on aggregation or network I/O.
#[derive(Clone, Copy)]
#[allow(clippy::module_name_repetitions)]
pub struct ServerlessStatsProcessor {}

#[async_trait]
impl StatsProcessor for ServerlessStatsProcessor {
    async fn process_stats(
        &self,
        req: Request,
        tx: Sender<pb::ClientStatsPayload>,
    ) -> Result<Response, Box<dyn std::error::Error + Send + Sync>> {
        debug!("Received trace stats to process");

        // Step 1: Extract and validate the request body
        let (parts, body) = match extract_request_body(req).await {
            Ok(r) => r,
            Err(e) => {
                let error_msg = format!("Error extracting request body: {e}");
                error!("{}", error_msg);
                return Ok((StatusCode::BAD_REQUEST, error_msg).into_response());
            }
        };

        // Step 2: Validate content-length header against MAX_CONTENT_LENGTH (50MB)
        // This prevents OOM from extremely large payloads
        if let Some(content_length) = parts.headers.get("content-length") {
            if let Ok(length_str) = content_length.to_str() {
                if let Ok(length) = length_str.parse::<usize>() {
                    if length > MAX_CONTENT_LENGTH {
                        let error_msg = format!(
                            "Content-Length {length} exceeds maximum allowed size {MAX_CONTENT_LENGTH}"
                        );
                        error!("{}", error_msg);
                        return Ok((StatusCode::PAYLOAD_TOO_LARGE, error_msg).into_response());
                    }
                }
            }
        }

        // Step 3: Deserialize msgpack-encoded stats to protobuf ClientStatsPayload
        // The client sends stats in msgpack format for efficiency
        let mut stats: pb::ClientStatsPayload =
            match stats_utils::get_stats_from_request_body(hyper_migration::Body::from_bytes(body))
                .await
            {
                Ok(result) => result,
                Err(err) => {
                    let error_msg =
                        format!("Error deserializing trace stats from request body: {err}");
                    error!("{}", error_msg);
                    return Ok((StatusCode::INTERNAL_SERVER_ERROR, error_msg).into_response());
                }
            };

        // Step 4: Set the timestamp for aggregation bucketing
        // The `start` field determines which time bucket the stats belong to
        // We use current Unix epoch time in nanoseconds for precise bucketing
        let start = SystemTime::now();
        let timestamp = start
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        stats.stats[0].start = if let Ok(result) = u64::try_from(timestamp) {
            result
        } else {
            let error_msg = "Error converting timestamp to u64";
            error!("{}", error_msg);
            return Ok((StatusCode::INTERNAL_SERVER_ERROR, error_msg).into_response());
        };

        // Step 5: Send stats to aggregator via async mpsc channel
        // This allows us to respond quickly (202 Accepted) without blocking on aggregation
        match tx.send(stats).await {
            Ok(()) => {
                debug!("Successfully buffered stats to be aggregated.");
                // Return 202 Accepted to indicate stats are queued for processing
                Ok((
                    StatusCode::ACCEPTED,
                    "Successfully buffered stats to be aggregated.",
                )
                    .into_response())
            }
            Err(err) => {
                // Channel send error likely means aggregator task has stopped
                let error_msg = format!("Error sending stats to the stats aggregator: {err}");
                error!("{}", error_msg);
                Ok((StatusCode::INTERNAL_SERVER_ERROR, error_msg).into_response())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{header::CONTENT_LENGTH, HeaderValue},
    };
    use bytes::Bytes;
    use tokio::sync::mpsc;

    /// Helper function to create a test processor
    fn create_test_processor() -> ServerlessStatsProcessor {
        ServerlessStatsProcessor {}
    }

    /// Helper function to create a valid stats payload in msgpack format
    fn create_valid_stats_payload() -> Bytes {
        // Create a minimal valid ClientStatsPayload in msgpack format
        // This is based on the structure expected by datadog-trace-protobuf
        let stats_payload = pb::ClientStatsPayload {
            hostname: "test-host".to_string(),
            env: "test-env".to_string(),
            version: "1.0.0".to_string(),
            stats: vec![pb::ClientStatsBucket {
                start: 0,                 // Will be overwritten by the processor
                duration: 10_000_000_000, // 10 seconds in nanoseconds
                stats: vec![],
                agent_time_shift: 0,
            }],
            lang: "rust".to_string(),
            tracer_version: "1.0.0".to_string(),
            runtime_id: "test-runtime-id".to_string(),
            sequence: 1,
            tags: vec![],
            service: "test-service".to_string(),
            container_id: "test-container".to_string(),
            git_commit_sha: String::new(),
            image_tag: String::new(),
            agent_aggregation: String::new(),
            process_tags: String::new(),
            process_tags_hash: 0,
        };

        // Serialize to msgpack
        let mut buf = Vec::new();
        rmp_serde::encode::write(&mut buf, &stats_payload)
            .expect("Failed to serialize stats payload");
        Bytes::from(buf)
    }

    /// Helper function to create a request with a body
    fn create_request_with_body(body: Bytes, content_length: Option<usize>) -> Request {
        let mut builder = Request::builder().method("POST").uri("/v0.6/stats");

        if let Some(length) = content_length {
            builder = builder.header(CONTENT_LENGTH, length.to_string());
        }

        builder.body(Body::from(body)).unwrap()
    }

    #[test]
    fn test_serverless_stats_processor_creation() {
        let processor = create_test_processor();
        // Verify the processor can be created
        // Since it's a unit struct, we can't inspect its fields
        let _copy = processor;
    }

    #[test]
    fn test_serverless_stats_processor_is_copy() {
        let processor = create_test_processor();
        let processor2 = processor;
        let _processor3 = processor2;
        // If this compiles, it proves Copy trait works
    }

    #[tokio::test]
    async fn test_process_stats_success() {
        let processor = create_test_processor();
        let (tx, mut rx) = mpsc::channel(10);

        let body = create_valid_stats_payload();
        let content_length = body.len();
        let req = create_request_with_body(body, Some(content_length));

        let result = processor.process_stats(req, tx).await;

        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        // Verify the stats were sent through the channel
        let received_stats = rx.recv().await;
        assert!(received_stats.is_some());
        let stats = received_stats.unwrap();
        assert_eq!(stats.hostname, "test-host");
        assert_eq!(stats.env, "test-env");
        assert_eq!(stats.service, "test-service");
        // Verify the timestamp was set (should be non-zero)
        assert!(stats.stats[0].start > 0);
    }

    #[tokio::test]
    async fn test_process_stats_content_length_too_large() {
        let processor = create_test_processor();
        let (tx, _rx) = mpsc::channel(10);

        let body = create_valid_stats_payload();
        // Set content-length to exceed MAX_CONTENT_LENGTH (50MB)
        let content_length = MAX_CONTENT_LENGTH + 1;
        let req = create_request_with_body(body, Some(content_length));

        let result = processor.process_stats(req, tx).await;

        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

        // Read response body to verify error message
        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();
        assert!(body_str.contains("exceeds maximum allowed size"));
    }

    #[tokio::test]
    async fn test_process_stats_content_length_at_max() {
        let processor = create_test_processor();
        let (tx, mut rx) = mpsc::channel(10);

        let body = create_valid_stats_payload();
        // Set content-length exactly at MAX_CONTENT_LENGTH
        let content_length = MAX_CONTENT_LENGTH;
        let req = create_request_with_body(body, Some(content_length));

        let result = processor.process_stats(req, tx).await;

        // Should succeed as it's at the limit, not exceeding it
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        // Verify the stats were sent
        let received_stats = rx.recv().await;
        assert!(received_stats.is_some());
    }

    #[tokio::test]
    async fn test_process_stats_no_content_length_header() {
        let processor = create_test_processor();
        let (tx, mut rx) = mpsc::channel(10);

        let body = create_valid_stats_payload();
        // Don't set content-length header
        let req = create_request_with_body(body, None);

        let result = processor.process_stats(req, tx).await;

        // Should succeed as missing content-length is allowed
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        // Verify the stats were sent
        let received_stats = rx.recv().await;
        assert!(received_stats.is_some());
    }

    #[tokio::test]
    async fn test_process_stats_invalid_msgpack() {
        let processor = create_test_processor();
        let (tx, _rx) = mpsc::channel(10);

        // Create an invalid msgpack body
        let body = Bytes::from("invalid msgpack data");
        let req = create_request_with_body(body, Some(20));

        let result = processor.process_stats(req, tx).await;

        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        // Read response body to verify error message
        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();
        assert!(body_str.contains("Error deserializing trace stats"));
    }

    #[tokio::test]
    async fn test_process_stats_empty_body() {
        let processor = create_test_processor();
        let (tx, _rx) = mpsc::channel(10);

        let body = Bytes::new();
        let req = create_request_with_body(body, Some(0));

        let result = processor.process_stats(req, tx).await;

        assert!(result.is_ok());
        let response = result.unwrap();
        // Empty body should fail deserialization
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn test_process_stats_channel_closed() {
        let processor = create_test_processor();
        let (tx, rx) = mpsc::channel(10);

        // Drop the receiver to close the channel
        drop(rx);

        let body = create_valid_stats_payload();
        let req = create_request_with_body(body, Some(100));

        let result = processor.process_stats(req, tx).await;

        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        // Read response body to verify error message
        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();
        assert!(body_str.contains("Error sending stats to the stats aggregator"));
    }

    #[tokio::test]
    async fn test_process_stats_multiple_requests() {
        let processor = create_test_processor();
        let (tx, mut rx) = mpsc::channel(10);

        // Send multiple requests
        for i in 0..3 {
            let body = create_valid_stats_payload();
            let req = create_request_with_body(body, Some(100));

            let result = processor.process_stats(req, tx.clone()).await;
            assert!(result.is_ok());

            let response = result.unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);

            // Verify each stats payload was sent
            let received_stats = rx.recv().await;
            assert!(received_stats.is_some(), "Request {} failed", i);
        }
    }

    #[tokio::test]
    async fn test_process_stats_timestamp_updated() {
        let processor = create_test_processor();
        let (tx, mut rx) = mpsc::channel(10);

        let body = create_valid_stats_payload();
        let req = create_request_with_body(body, Some(100));

        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let result = processor.process_stats(req, tx).await;
        assert!(result.is_ok());

        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        // Verify the stats were sent and timestamp was updated
        let received_stats = rx.recv().await;
        assert!(received_stats.is_some());
        let stats = received_stats.unwrap();

        let timestamp = stats.stats[0].start as u128;
        // Verify timestamp is within the expected range
        assert!(
            timestamp >= before && timestamp <= after,
            "Timestamp {} should be between {} and {}",
            timestamp,
            before,
            after
        );
    }

    #[tokio::test]
    async fn test_process_stats_invalid_content_length_header() {
        let processor = create_test_processor();
        let (tx, mut rx) = mpsc::channel(10);

        let body = create_valid_stats_payload();
        let mut req = create_request_with_body(body, None);

        // Add an invalid content-length header (non-numeric)
        req.headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from_static("not-a-number"));

        let result = processor.process_stats(req, tx).await;

        // Should succeed as invalid content-length is ignored
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        // Verify the stats were sent
        let received_stats = rx.recv().await;
        assert!(received_stats.is_some());
    }

    #[tokio::test]
    async fn test_process_stats_preserves_original_payload_data() {
        let processor = create_test_processor();
        let (tx, mut rx) = mpsc::channel(10);

        let body = create_valid_stats_payload();
        let req = create_request_with_body(body, Some(100));

        let result = processor.process_stats(req, tx).await;
        assert!(result.is_ok());

        let received_stats = rx.recv().await.unwrap();

        // Verify all original fields are preserved
        assert_eq!(received_stats.hostname, "test-host");
        assert_eq!(received_stats.env, "test-env");
        assert_eq!(received_stats.version, "1.0.0");
        assert_eq!(received_stats.lang, "rust");
        assert_eq!(received_stats.tracer_version, "1.0.0");
        assert_eq!(received_stats.runtime_id, "test-runtime-id");
        assert_eq!(received_stats.sequence, 1);
        assert_eq!(received_stats.service, "test-service");
        assert_eq!(received_stats.container_id, "test-container");
    }

    #[tokio::test]
    async fn test_process_stats_response_body_accepted() {
        let processor = create_test_processor();
        let (tx, _rx) = mpsc::channel(10);

        let body = create_valid_stats_payload();
        let req = create_request_with_body(body, Some(100));

        let result = processor.process_stats(req, tx).await;
        assert!(result.is_ok());

        let response = result.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        // Verify response body contains success message
        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();
        assert_eq!(body_str, "Successfully buffered stats to be aggregated.");
    }

    #[tokio::test]
    async fn test_process_stats_with_small_channel_buffer() {
        let processor = create_test_processor();
        // Create a channel with buffer size of 1
        let (tx, mut rx) = mpsc::channel(1);

        let body = create_valid_stats_payload();
        let req = create_request_with_body(body, Some(100));

        let result = processor.process_stats(req, tx).await;
        assert!(result.is_ok());

        let response = result.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        // Verify stats were received
        let received_stats = rx.recv().await;
        assert!(received_stats.is_some());
    }

    #[test]
    fn test_stats_processor_trait_object_safety() {
        // Verify the trait is object-safe by creating a trait object
        let processor = ServerlessStatsProcessor {};
        let _trait_object: &dyn StatsProcessor = &processor;
    }
}
