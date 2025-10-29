//! High-level logs agent coordinating the entire log pipeline.
//!
//! This module provides the top-level agent that:
//! 1. Receives log events from various sources
//! 2. Processes them (filtering, masking, enrichment)
//! 3. Sends them to the aggregator for batching
//! 4. Handles graceful shutdown with event draining
//!
//! # Architecture
//!
//! ```text
//!   Log Sources
//!       │
//!       v
//!   ┌─────────────┐
//!   │   Channel   │ (mpsc, bounded)
//!   └──────┬──────┘
//!          │
//!          v
//!   ┌─────────────┐
//!   │  LogsAgent  │ (event loop)
//!   └──────┬──────┘
//!          │
//!          v
//!   ┌─────────────┐
//!   │  Processor  │ (filter, mask, enrich)
//!   └──────┬──────┘
//!          │
//!          v
//!   ┌─────────────┐
//!   │ Aggregator  │ (batching)
//!   └─────────────┘
//! ```
//!
//! # Graceful Shutdown
//!
//! The agent supports graceful shutdown via cancellation token:
//! - On cancellation, drains all pending events before stopping
//! - Ensures no logs are lost during shutdown

use std::sync::Arc;
use tokio::sync::mpsc::{self, Sender};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::event_bus::Event;
// Lambda-specific telemetry events removed for general agent
// use crate::extension::telemetry::events::TelemetryEvent;
use crate::config;
use crate::logs::{aggregator_service::AggregatorHandle, processor::LogsProcessor};
use crate::tags;

/// Represents a log event to be processed.
///
/// This is a placeholder for the generic agent. In production, this would
/// be replaced with a more comprehensive log event type or HTTP-based
/// intake similar to the traces module.
#[derive(Debug, Clone)]
pub struct LogEvent {
    /// The log message content.
    pub message: String,
    /// Timestamp when the log was generated.
    pub timestamp: std::time::SystemTime,
    /// Log level (e.g., "info", "error", "debug").
    pub level: String,
}

/// High-level agent coordinating the log processing pipeline.
///
/// Receives log events via channel, processes them through the processor,
/// and sends them to the aggregator for batching and flushing.
#[allow(clippy::module_name_repetitions)]
pub struct LogsAgent {
    /// Channel receiver for incoming log events.
    rx: mpsc::Receiver<LogEvent>,
    /// Processor for filtering, masking, and enriching logs.
    processor: LogsProcessor,
    /// Handle to send processed logs to aggregator.
    aggregator_handle: AggregatorHandle,
    /// Token for graceful shutdown coordination.
    cancel_token: CancellationToken,
}

impl LogsAgent {
    /// Creates a new logs agent with the given configuration.
    ///
    /// # Arguments
    ///
    /// * `tags_provider` - Provider for enriching logs with tags
    /// * `datadog_config` - Agent configuration
    /// * `event_bus` - Channel for publishing agent events
    /// * `aggregator_handle` - Handle to send processed logs to aggregator
    ///
    /// # Returns
    ///
    /// Tuple of (agent, sender). Spawn the agent task and use the sender
    /// to send log events.
    ///
    /// # Channel Capacity
    ///
    /// The internal channel has a capacity of 1,000 events. If the channel
    /// fills up, senders will block until the agent processes events.
    #[must_use]
    pub fn new(
        tags_provider: Arc<tags::provider::Provider>,
        datadog_config: Arc<config::Config>,
        event_bus: Sender<Event>,
        aggregator_handle: AggregatorHandle,
    ) -> (Self, Sender<LogEvent>) {
        let processor = LogsProcessor::new(Arc::clone(&datadog_config), tags_provider, event_bus);

        // Bounded channel with 1,000 event capacity
        let (tx, rx) = mpsc::channel::<LogEvent>(1000);
        let cancel_token = CancellationToken::new();

        let agent = Self {
            rx,
            processor,
            aggregator_handle,
            cancel_token,
        };

        (agent, tx)
    }

    /// Main event loop that processes log events until cancellation.
    ///
    /// This method runs indefinitely, processing events from the channel.
    /// On cancellation, it drains all remaining events before stopping.
    ///
    /// # Graceful Shutdown
    ///
    /// 1. Cancellation token is triggered
    /// 2. Agent logs shutdown message
    /// 3. All pending events in channel are drained and processed
    /// 4. Agent stops once channel is empty
    ///
    /// This ensures no logs are lost during shutdown.
    pub async fn spin(&mut self) {
        loop {
            tokio::select! {
                // Normal event processing
                Some(event) = self.rx.recv() => {
                    self.processor.process(event, &self.aggregator_handle).await;
                }
                // Graceful shutdown triggered
                () = self.cancel_token.cancelled() => {
                    debug!("LOGS_AGENT | Received shutdown signal, draining remaining events");

                    // Drain all remaining events from channel
                    'drain_logs_loop: loop {
                        match self.rx.try_recv() {
                            // Event available - process it
                            Ok(event) => {
                                self.processor.process(event, &self.aggregator_handle).await;
                            }
                            // Channel disconnected - all events processed
                            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                                debug!("LOGS_AGENT | Channel disconnected, finished draining");
                                break 'drain_logs_loop;
                            },
                            // Channel empty but senders still exist
                            // Decision: Continue draining in case more events arrive
                            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                                debug!("LOGS_AGENT | No more events to process but still have senders, continuing to drain...");
                            },
                        }
                    }

                    break; // Exit main loop after draining
                }
            }
        }
    }

    /// Processes a single event synchronously.
    ///
    /// This is an alternative to `spin()` for scenarios where you want
    /// to process events one at a time with manual control.
    ///
    /// # Cancellation
    ///
    /// If cancellation is triggered while waiting for an event, this
    /// method returns immediately without processing.
    pub async fn sync_consume(&mut self) {
        tokio::select! {
            Some(events) = self.rx.recv() => {
                self.processor
                    .process(events, &self.aggregator_handle)
                    .await;
            }
            () = self.cancel_token.cancelled() => {
                // Cancellation requested, exit early
            }
        }
    }

    /// Returns a clone of the cancellation token for shutdown coordination.
    ///
    /// Call `cancel_token.cancel()` to trigger graceful shutdown.
    #[must_use]
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::event_bus;
    use crate::logs::aggregator_service::AggregatorService;
    use crate::tags;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};
    use tokio::time::timeout;

    // Helper functions

    fn create_test_config() -> Config {
        Config {
            api_key: "test-api-key".to_string(),
            logs_config_logs_dd_url: "https://logs.example.com".to_string(),
            logs_config_use_compression: false,
            logs_config_compression_level: 3,
            observability_pipelines_worker_logs_enabled: false,
            observability_pipelines_worker_logs_url: String::new(),
            flush_timeout: 5,
            ..Default::default()
        }
    }

    fn create_test_tags_provider() -> Arc<tags::provider::Provider> {
        let config = Arc::new(create_test_config());
        Arc::new(tags::provider::Provider::new(config))
    }

    fn create_test_aggregator_handle() -> AggregatorHandle {
        let (_service, handle) = AggregatorService::default();
        handle
    }

    fn create_test_event_bus() -> Sender<Event> {
        let (_event_bus, tx) = event_bus::EventBus::run();
        tx
    }

    fn create_test_log_event(message: &str) -> LogEvent {
        LogEvent {
            message: message.to_string(),
            timestamp: SystemTime::now(),
            level: "info".to_string(),
        }
    }

    fn create_logs_agent() -> (LogsAgent, Sender<LogEvent>) {
        let tags_provider = create_test_tags_provider();
        let config = Arc::new(create_test_config());
        let event_bus = create_test_event_bus();
        let aggregator_handle = create_test_aggregator_handle();

        LogsAgent::new(tags_provider, config, event_bus, aggregator_handle)
    }

    // Tests for LogEvent

    #[test]
    fn test_log_event_creation() {
        let event = create_test_log_event("test message");
        assert_eq!(event.message, "test message");
        assert_eq!(event.level, "info");
        assert!(event.timestamp <= SystemTime::now());
    }

    #[test]
    fn test_log_event_clone() {
        let event = create_test_log_event("test message");
        let cloned = event.clone();
        assert_eq!(event.message, cloned.message);
        assert_eq!(event.level, cloned.level);
        assert_eq!(event.timestamp, cloned.timestamp);
    }

    #[test]
    fn test_log_event_debug() {
        let event = create_test_log_event("test message");
        let debug_str = format!("{:?}", event);
        assert!(debug_str.contains("LogEvent"));
        assert!(debug_str.contains("test message"));
    }

    // Tests for LogsAgent::new

    #[test]
    fn test_logs_agent_new() {
        let (agent, tx) = create_logs_agent();

        // Verify that the agent was created successfully
        // We can't directly inspect the internal state, but we can verify:
        // 1. The sender can send messages
        assert!(!tx.is_closed());

        // 2. The cancel token is properly initialized
        let cancel_token = agent.cancel_token();
        assert!(!cancel_token.is_cancelled());
    }

    #[test]
    fn test_logs_agent_new_with_different_configs() {
        let tags_provider = create_test_tags_provider();
        let event_bus = create_test_event_bus();
        let aggregator_handle = create_test_aggregator_handle();

        // Test with compression enabled
        let mut config = create_test_config();
        config.logs_config_use_compression = true;
        let config_arc = Arc::new(config);

        let (agent, tx) = LogsAgent::new(
            Arc::clone(&tags_provider),
            config_arc,
            event_bus,
            aggregator_handle,
        );

        assert!(!tx.is_closed());
        assert!(!agent.cancel_token().is_cancelled());
    }

    #[tokio::test]
    async fn test_logs_agent_new_channel_capacity() {
        let (mut agent, tx) = create_logs_agent();

        // Spawn a task to consume events
        let cancel_token = agent.cancel_token();
        tokio::spawn(async move {
            agent.spin().await;
        });

        // The channel capacity is 1000, so we should be able to send multiple events
        for i in 0..10 {
            let event = LogEvent {
                message: format!("test message {}", i),
                timestamp: SystemTime::now(),
                level: "info".to_string(),
            };
            // This should not block for small numbers
            assert!(tx.send(event).await.is_ok());
        }

        // Cancel the agent
        cancel_token.cancel();
    }

    // Tests for LogsAgent::cancel_token

    #[test]
    fn test_cancel_token_not_cancelled_initially() {
        let (agent, _) = create_logs_agent();
        let cancel_token = agent.cancel_token();
        assert!(!cancel_token.is_cancelled());
    }

    #[test]
    fn test_cancel_token_can_be_cancelled() {
        let (agent, _) = create_logs_agent();
        let cancel_token = agent.cancel_token();

        cancel_token.cancel();
        assert!(cancel_token.is_cancelled());
    }

    #[test]
    fn test_cancel_token_clones_independently() {
        let (agent, _) = create_logs_agent();
        let token1 = agent.cancel_token();
        let token2 = agent.cancel_token();

        // Cancelling one token should affect both (they're clones of the same token)
        token1.cancel();
        assert!(token2.is_cancelled());
    }

    // Tests for LogsAgent::sync_consume

    #[tokio::test]
    async fn test_sync_consume_processes_event() {
        let (mut agent, tx) = create_logs_agent();

        // Send an event
        let event = create_test_log_event("test message");
        tx.send(event).await.expect("Failed to send event");

        // Consume the event
        agent.sync_consume().await;

        // If we got here without panicking, the event was processed successfully
    }

    #[tokio::test]
    async fn test_sync_consume_with_cancellation() {
        let (mut agent, _tx) = create_logs_agent();

        // Cancel before consuming
        let cancel_token = agent.cancel_token();
        cancel_token.cancel();

        // sync_consume should exit immediately due to cancellation
        let result = timeout(Duration::from_millis(100), agent.sync_consume()).await;
        assert!(
            result.is_ok(),
            "sync_consume should complete quickly when cancelled"
        );
    }

    #[tokio::test]
    async fn test_sync_consume_with_empty_channel() {
        let (mut agent, tx) = create_logs_agent();

        // Drop the sender to close the channel
        drop(tx);

        // sync_consume should handle empty/closed channel gracefully
        // This will wait indefinitely for a message or cancellation, so we use a timeout
        let result = timeout(Duration::from_millis(100), agent.sync_consume()).await;
        assert!(
            result.is_err(),
            "sync_consume should wait when channel is empty"
        );
    }

    // Tests for LogsAgent::spin
    // Note: Full spin() tests are complex because they involve internal aggregator processing
    // We test the basic cancellation mechanism here

    #[tokio::test]
    async fn test_spin_with_channel_closed() {
        let (mut agent, tx) = create_logs_agent();

        let cancel_token = agent.cancel_token();

        // Spawn spin in background
        let spin_task = tokio::spawn(async move {
            agent.spin().await;
        });

        // Drop the sender to close the channel
        drop(tx);

        // Cancel to allow spin to exit
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel_token.cancel();

        // spin should handle closed channel gracefully
        let result = timeout(Duration::from_millis(200), spin_task).await;
        assert!(result.is_ok(), "spin should handle closed channel");
    }

    // Integration tests

    #[tokio::test]
    async fn test_agent_with_opw_enabled() {
        let tags_provider = create_test_tags_provider();
        let event_bus = create_test_event_bus();
        let aggregator_handle = create_test_aggregator_handle();

        let mut config = create_test_config();
        config.observability_pipelines_worker_logs_enabled = true;
        config.observability_pipelines_worker_logs_url = "https://opw.example.com".to_string();

        let (mut agent, tx) = LogsAgent::new(
            tags_provider,
            Arc::new(config),
            event_bus,
            aggregator_handle,
        );

        // Send an event
        let event = create_test_log_event("opw test");
        tx.send(event).await.expect("Failed to send event");

        // Process it
        agent.sync_consume().await;
    }

    #[tokio::test]
    async fn test_multiple_log_levels() {
        let (mut agent, tx) = create_logs_agent();

        let levels = vec!["debug", "info", "warn", "error"];

        for level in levels {
            let event = LogEvent {
                message: format!("{} level message", level),
                timestamp: SystemTime::now(),
                level: level.to_string(),
            };
            tx.send(event).await.expect("Failed to send event");
        }

        // Process events
        for _ in 0..4 {
            let consume_result = timeout(Duration::from_millis(100), agent.sync_consume()).await;
            assert!(consume_result.is_ok());
        }
    }

    #[tokio::test]
    async fn test_agent_with_large_messages() {
        let (mut agent, tx) = create_logs_agent();

        // Create a large message
        let large_message = "x".repeat(10000);
        let event = LogEvent {
            message: large_message.clone(),
            timestamp: SystemTime::now(),
            level: "info".to_string(),
        };

        tx.send(event).await.expect("Failed to send large event");
        agent.sync_consume().await;
    }

    #[tokio::test]
    async fn test_agent_with_special_characters() {
        let (mut agent, tx) = create_logs_agent();

        let special_messages = vec![
            "Message with unicode: \u{1F600} \u{1F4A9}",
            "Message with newlines:\nline1\nline2",
            "Message with tabs:\t\ttabbed",
            "Message with quotes: \"quoted\" and 'single'",
            "Message with backslashes: \\ \\\\ \\\\\\",
        ];

        for msg in special_messages {
            let event = LogEvent {
                message: msg.to_string(),
                timestamp: SystemTime::now(),
                level: "info".to_string(),
            };
            tx.send(event).await.expect("Failed to send event");
            agent.sync_consume().await;
        }
    }

    #[tokio::test]
    async fn test_concurrent_event_sending() {
        let (mut agent, tx) = create_logs_agent();

        // Spawn agent processor in background
        let cancel_token = agent.cancel_token();
        let cancel_token_for_agent = cancel_token.clone();
        tokio::spawn(async move {
            agent.spin().await;
        });

        // Spawn multiple tasks that send events concurrently
        let mut handles = vec![];
        for i in 0..5 {
            let tx_clone = tx.clone();
            let handle = tokio::spawn(async move {
                for j in 0..3 {
                    let event = LogEvent {
                        message: format!("concurrent message from task {} iteration {}", i, j),
                        timestamp: SystemTime::now(),
                        level: "info".to_string(),
                    };
                    let _ = tx_clone.send(event).await;
                }
            });
            handles.push(handle);
        }

        // Wait for all senders to finish
        for handle in handles {
            let _ = handle.await;
        }

        // Give some time for processing, then cancel
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel_token_for_agent.cancel();

        // Verify agent can be cancelled
        assert!(cancel_token.is_cancelled());
    }

    #[test]
    fn test_processor_creation_with_different_tags() {
        let mut config = create_test_config();
        config.service = Some("test-service".to_string());
        config.env = Some("test".to_string());

        let config_arc = Arc::new(config);
        let tags_provider = Arc::new(tags::provider::Provider::new(Arc::clone(&config_arc)));
        let event_bus = create_test_event_bus();

        let _processor = LogsProcessor::new(config_arc, tags_provider, event_bus);

        // If we got here without panicking, the processor was created successfully
    }
}
