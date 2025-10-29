//! Log collection and forwarding to Datadog.
//!
//! This module provides a complete log ingestion pipeline that collects logs
//! from various sources, processes them, batches them efficiently, and forwards
//! them to Datadog's log intake API.
//!
//! # Architecture
//!
//! The logs subsystem implements a multi-stage pipeline:
//!
//! ```text
//!                     ┌──────────────┐
//!                     │   Sources    │  (Applications, System, Files)
//!                     └──────┬───────┘
//!                            │
//!                            v
//!                     ┌──────────────┐
//!                     │  Processor   │  (Parse, enrich, validate)
//!                     └──────┬───────┘
//!                            │
//!                            v
//!                  ┌─────────────────┐
//!                  │   Aggregator    │  (Buffer, batch by size/count)
//!                  │    Service      │
//!                  └─────────┬───────┘
//!                            │
//!                            v
//!                     ┌──────────────┐
//!                     │   Flusher    │  (Compress, send HTTP POST)
//!                     └──────┬───────┘
//!                            │
//!                            v
//!                  ┌─────────────────┐
//!                  │ Datadog Intake  │
//!                  └─────────────────┘
//! ```
//!
//! # Components
//!
//! - **[`agent`]**: High-level log agent coordinating the entire pipeline
//! - **[`processor`]**: Parses, enriches, and validates raw log entries
//! - **[`aggregator`]**: Buffers and batches logs based on size/count limits
//! - **[`aggregator_service`]**: Actor-based service for concurrent log aggregation
//! - **[`flusher`]**: Compresses and sends batched logs to Datadog via HTTP
//! - **[`constants`]**: API limits for payload sizes and batch counts
//!
//! # Data Flow
//!
//! 1. **Ingestion**: Logs arrive from various sources (HTTP endpoints, files, etc.)
//! 2. **Processing**: Raw logs are parsed into structured format with enrichment
//! 3. **Aggregation**: Processed logs are buffered and batched for efficiency
//! 4. **Flushing**: Batches are compressed and sent to Datadog intake API
//! 5. **Delivery**: Successful delivery confirmed, failed logs can be retried
//!
//! # Memory Management
//!
//! The logs pipeline implements multiple levels of backpressure:
//! - **Entry size limits**: Individual logs truncated at 1MB
//! - **Batch size limits**: Batches limited to 5MB uncompressed
//! - **Batch count limits**: Maximum 1,000 entries per batch
//! - **Queue limits**: Bounded buffers prevent unbounded memory growth
//!
//! # Performance Characteristics
//!
//! - **Throughput**: Designed for high-volume log ingestion
//! - **Latency**: Batching introduces intentional delay for efficiency
//! - **Memory**: Bounded by queue limits and size constraints
//! - **CPU**: Minimal processing overhead (JSON parsing, compression)

pub mod agent;
pub mod aggregator;
pub mod aggregator_service;
pub mod constants;
pub mod flusher;
// Lambda-specific module removed for general agent
// pub mod lambda;
pub mod processor;
