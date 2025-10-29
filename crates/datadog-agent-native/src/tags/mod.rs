//! Tag management and provider functionality.
//!
//! This module provides utilities for managing tags that are attached to
//! telemetry data (traces, logs, metrics) sent to Datadog.
//!
//! # Tag Sources
//!
//! Tags can come from multiple sources:
//! - Environment variables (`DD_TAGS`, `DD_ENV`, `DD_SERVICE`, `DD_VERSION`)
//! - Lambda execution context (function name, ARN, version, etc.)
//! - Container metadata (ECS task ARN, fargate information)
//! - Cloud provider metadata (AWS region, availability zone)
//!
//! # Tag Provider
//!
//! The [`provider`] module contains the core tag provider implementation that
//! collects tags from all sources and makes them available to the agent.
//!
//! # Example
//!
//! ```rust,ignore
//! use datadog_agent_native::tags::provider::TagProvider;
//!
//! let provider = TagProvider::new();
//! let tags = provider.get_tags();
//! println!("Tags: {:?}", tags);
//! ```

pub mod provider;
