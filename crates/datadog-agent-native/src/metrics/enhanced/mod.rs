//! Enhanced metrics implementation for resource monitoring.
//!
//! This module provides the core implementation for collecting enhanced metrics
//! beyond standard Lambda runtime metrics. It includes:
//!
//! - **constants**: Metric names and configuration values
//! - **statfs**: Filesystem statistics for `/tmp` directory monitoring
//! - **usage_metrics**: Actor-pattern service for tracking high-water mark metrics
//!
//! # Architecture
//!
//! The enhanced metrics system uses an actor pattern for thread-safe metric updates:
//!
//! ```text
//! Monitoring Loop          EnhancedMetricsService (Actor)       Reporter
//!       │                            │                              │
//!       ├─ poll /tmp usage           │                              │
//!       ├─ poll fd count             │                              │
//!       ├─ poll thread count         │                              │
//!       │                            │                              │
//!       ├──update_metrics()────────>│                              │
//!       │                            ├─ track high-water marks      │
//!       │                            │   (max values only)          │
//!       │                            │                              │
//!       │                            │      <───get_metrics()───────┤
//!       │                            ├────── return metrics ───────>│
//!       │                            │                              │
//!       │      <───reset_metrics()───┤ (after flush)               │
//!       │                            │                              │
//! ```
//!
//! # Lambda-Specific Features
//!
//! The original Lambda-specific module (`lambda`) has been removed for the
//! general agent build. It can be restored via feature flags for Lambda deployments.

pub mod constants;
// Lambda-specific module removed for general agent
// pub mod lambda;
pub mod statfs;
pub mod usage_metrics;
