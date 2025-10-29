// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! APM (Application Performance Monitoring) trace processing and forwarding.
//!
//! This module implements the Datadog APM trace agent functionality, responsible for:
//! - **Trace Collection**: Receiving traces from instrumented applications via HTTP and UDS
//! - **Trace Processing**: Enriching traces with tags, normalizing service names, obfuscating sensitive data
//! - **Stats Computation**: Generating trace statistics and aggregates for APM metrics
//! - **Context Propagation**: Supporting distributed tracing across service boundaries
//! - **Forwarding**: Sending processed traces and stats to Datadog intake endpoints
//!
//! # Architecture
//!
//! The traces module consists of several key components:
//!
//! ## Core Components
//! - **`trace_agent`**: Main orchestrator coordinating trace collection and processing
//! - **`trace_processor`**: Processes individual traces (tagging, obfuscation, normalization)
//! - **`trace_aggregator`**: Buffers and batches traces for efficient forwarding
//! - **`trace_flusher`**: Sends trace batches to Datadog intake endpoints
//!
//! ## Statistics Components
//! - **`stats_processor`**: Computes trace statistics (latency, error rates, throughput)
//! - **`stats_aggregator`**: Aggregates statistics across time windows
//! - **`stats_concentrator_service`**: Manages stats computation lifecycle
//! - **`stats_generator`**: Generates APM metrics from trace statistics
//! - **`stats_flusher`**: Sends statistics to Datadog
//!
//! ## Proxy Components
//! - **`proxy_aggregator`**: Aggregates traces from multiple sources
//! - **`proxy_flusher`**: Forwards traces with minimal processing overhead
//!
//! ## Support Components
//! - **`context`**: Trace and span context structures for distributed tracing
//! - **`propagation`**: Distributed trace context propagation (Datadog, B3, W3C)
//! - **`container`**: Container ID extraction and tagging
//! - **`tagger`**: Entity tagging for enriching traces with orchestrator metadata
//! - **`span_pointers`**: Efficient span reference management
//! - **`uds`**: Unix Domain Socket support for local trace collection
//!
//! # Trace Flow
//!
//! ```text
//! Applications (instrumented with Datadog tracers)
//!   ↓
//! HTTP/UDS Endpoints (trace_agent receives traces)
//!   ↓
//! Trace Processor (enrichment, normalization, obfuscation)
//!   ↓
//! Trace Aggregator (batching) + Stats Processor (metrics computation)
//!   ↓
//! Trace Flusher (send to Datadog) + Stats Flusher (send stats)
//! ```
//!
//! # Distributed Tracing
//!
//! The module supports multiple propagation styles for cross-service tracing:
//! - **Datadog native**: `x-datadog-*` headers
//! - **B3 (Zipkin)**: Multi-header and single-header formats
//! - **W3C Trace Context**: `traceparent` / `tracestate` headers
//!
//! See `propagation` module for details on context propagation.

pub mod container;
pub mod context;
pub mod propagation;
pub mod proxy_aggregator;
pub mod proxy_flusher;
pub mod span_pointers;
pub mod stats_aggregator;
pub mod stats_concentrator_service;
pub mod stats_flusher;
pub mod stats_generator;
pub mod stats_processor;
pub mod tagger;
pub mod trace_agent;
pub mod trace_aggregator;
pub mod trace_flusher;
pub mod trace_processor;
pub mod uds;

/// Time conversion constant: seconds to milliseconds.
///
/// Used throughout the traces module for converting time values between
/// seconds and milliseconds representations.
///
/// # Value
/// `1_000` milliseconds per second
pub(crate) const S_TO_MS: u64 = 1_000;
