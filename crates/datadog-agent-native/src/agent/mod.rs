//! Agent Coordinator Module
//!
//! This module provides the `AgentCoordinator` which manages the lifecycle of all
//! agent components in a unified way. It coordinates:
//!
//! - Trace Agent (APM)
//! - Logs Agent
//! - Metrics Agent (DogStatsD)
//! - Remote Configuration Service
//! - AppSec Processor
//! - OTLP Receivers
//! - Event Bus
//!
//! ## Architecture
//!
//! The coordinator follows a staged startup/shutdown pattern:
//!
//! 1. **Initialization**: Create all components with shared resources
//! 2. **Startup**: Start all services in dependency order
//! 3. **Running**: All services operate concurrently
//! 4. **Shutdown**: Graceful shutdown in reverse dependency order
//!
//! ## Usage
//!
//! ```rust,no_run
//! use datadog_agent_native::agent::AgentCoordinator;
//! use datadog_agent_native::config::Config;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Load configuration
//! let config = Config::default();
//!
//! // Create coordinator
//! let mut coordinator = AgentCoordinator::new(config)?;
//!
//! // Start all services
//! coordinator.start().await?;
//!
//! // Wait for shutdown signal (Ctrl+C, SIGTERM, etc.)
//! coordinator.wait_for_shutdown().await;
//!
//! // Graceful shutdown
//! coordinator.shutdown().await?;
//! # Ok(())
//! # }
//! ```

pub mod coordinator;

pub use coordinator::{AgentCoordinator, AgentError, AgentHandle, ShutdownReason};
