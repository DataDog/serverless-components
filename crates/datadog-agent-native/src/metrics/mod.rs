//! Enhanced metrics collection for Lambda functions and serverless applications.
//!
//! This module provides infrastructure for collecting and reporting enhanced metrics
//! that go beyond the standard metrics available from the AWS Lambda runtime. These
//! metrics provide deeper insights into Lambda function performance and resource usage.
//!
//! # Enhanced Metrics Categories
//!
//! - **Memory**: Max memory used, memory size, memory utilization
//! - **CPU**: System/user time, total utilization, core count
//! - **Duration**: Runtime, billed, post-runtime, initialization durations
//! - **Network**: RX/TX bytes, total network throughput
//! - **Filesystem**: /tmp directory usage (max, used, free)
//! - **Resources**: File descriptor and thread usage
//! - **Errors**: Out-of-memory events, timeouts, general errors
//! - **Cost**: Estimated Lambda execution cost
//!
//! # Module Structure
//!
//! The enhanced metrics system consists of:
//! - **constants**: Metric names and configuration constants
//! - **statfs**: Filesystem statistics (/tmp directory monitoring)
//! - **usage_metrics**: Actor-based service for tracking high-water marks
//!
//! # Usage Pattern
//!
//! ```rust,ignore
//! use datadog_agent_native::metrics::enhanced::usage_metrics::EnhancedMetricsService;
//!
//! // Create service and handle
//! let (service, handle) = EnhancedMetricsService::new();
//!
//! // Spawn service task
//! tokio::spawn(async move { service.run().await });
//!
//! // Update metrics during execution
//! handle.update_metrics(Some(tmp_used), Some(fd_count), Some(thread_count))?;
//!
//! // Retrieve metrics for reporting
//! let metrics = handle.get_metrics().await?;
//! ```
//!
//! # Feature: Lambda-Specific Metrics
//!
//! The general agent focuses on core metrics. Lambda-specific features
//! (invocation tracking, response latency) are removed in this build but
//! can be enabled via feature flags for Lambda deployments.

pub mod enhanced;
