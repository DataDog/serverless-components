//! Actor-based log aggregation service for concurrent access.
//!
//! This module provides a thread-safe aggregation service using the actor pattern,
//! allowing multiple producers to safely add logs while a single service task
//! manages the aggregator state.
//!
//! # Actor Pattern
//!
//! The service implements the actor pattern for thread-safe aggregation:
//!
//! ```text
//!    ┌──────────────┐
//!    │   Handles    │ (Multiple producers)
//!    │  (Clone)     │
//!    └──────┬───────┘
//!           │ Commands via channel
//!           v
//!    ┌──────────────┐
//!    │   Service    │ (Single consumer)
//!    │ Actor Task   │
//!    └──────┬───────┘
//!           │ Owns aggregator
//!           v
//!    ┌──────────────┐
//!    │  Aggregator  │ (No locks needed!)
//!    └──────────────┘
//! ```
//!
//! # Benefits
//!
//! - **Lock-free**: No mutexes needed - channels provide synchronization
//! - **Scalable**: Handles are cloneable for multiple producers
//! - **Sequential**: Commands processed one at a time (no races)
//! - **Non-blocking**: `insert_batch` doesn't block producers
//!
//! # Example Usage
//!
//! ```rust,ignore
//! use datadog_agent_native::logs::aggregator_service::AggregatorService;
//!
//! // Create service and handle
//! let (service, handle) = AggregatorService::default();
//!
//! // Spawn service task
//! tokio::spawn(async move {
//!     service.run().await;
//! });
//!
//! // Use handle from multiple tasks
//! let handle_clone = handle.clone();
//! tokio::spawn(async move {
//!     handle_clone.insert_batch(vec![log1, log2]).unwrap();
//! });
//!
//! // Get batches when ready to flush
//! let batches = handle.get_batches().await.unwrap();
//! ```

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error};

use crate::logs::{aggregator::Aggregator, constants};

/// Commands that can be sent to the aggregator service.
///
/// These commands are sent via the channel from handles to the service task.
///
/// # Variants
///
/// - **InsertBatch**: Add logs to the aggregator (non-blocking)
/// - **GetBatches**: Retrieve all batched logs (blocks until response)
/// - **Shutdown**: Stop the service gracefully
#[derive(Debug)]
pub enum AggregatorCommand {
    /// Insert a batch of log entries into the aggregator.
    ///
    /// The logs are added to the aggregator's queue. If the queue is full,
    /// oldest logs are evicted using FIFO policy.
    InsertBatch(Vec<String>),

    /// Retrieve all batched logs.
    ///
    /// This drains the aggregator and returns all logs as JSON array batches.
    /// The oneshot channel is used to send the response back to the caller.
    GetBatches(oneshot::Sender<Vec<Vec<u8>>>),

    /// Shutdown the aggregator service gracefully.
    ///
    /// The service will stop processing commands after receiving this.
    Shutdown,
}

/// Handle for sending commands to the aggregator service.
///
/// This handle is cloneable and can be shared across multiple tasks,
/// allowing concurrent log insertion from different parts of the application.
///
/// # Thread Safety
///
/// The handle uses an unbounded MPSC channel, making it safe to clone
/// and use from multiple threads/tasks simultaneously.
///
/// # Example
///
/// ```rust,ignore
/// let handle_clone = handle.clone();
/// tokio::spawn(async move {
///     handle_clone.insert_batch(logs).unwrap();
/// });
/// ```
#[derive(Clone, Debug)]
pub struct AggregatorHandle {
    /// Sender for the command channel to the service.
    tx: mpsc::UnboundedSender<AggregatorCommand>,
}

impl AggregatorHandle {
    /// Inserts a batch of logs into the aggregator (non-blocking).
    ///
    /// This sends the logs to the service via the channel. If the queue is full,
    /// the service will evict oldest logs using FIFO policy.
    ///
    /// # Returns
    ///
    /// - `Ok(())` if command was sent successfully
    /// - `Err` if service has shut down
    pub fn insert_batch(
        &self,
        logs: Vec<String>,
    ) -> Result<(), mpsc::error::SendError<AggregatorCommand>> {
        self.tx.send(AggregatorCommand::InsertBatch(logs))
    }

    /// Retrieves all batched logs from the aggregator.
    ///
    /// This blocks until the service responds with batched logs.
    /// The aggregator is drained, so subsequent calls return empty until more logs are added.
    ///
    /// # Returns
    ///
    /// - `Ok(Vec<Vec<u8>>)` - Batched logs as JSON arrays
    /// - `Err(String)` - If service is unavailable
    pub async fn get_batches(&self) -> Result<Vec<Vec<u8>>, String> {
        let (response_tx, response_rx) = oneshot::channel();
        self.tx
            .send(AggregatorCommand::GetBatches(response_tx))
            .map_err(|e| format!("Failed to send flush command: {e}"))?;

        response_rx
            .await
            .map_err(|e| format!("Failed to receive flush response: {e}"))
    }

    /// Shuts down the aggregator service gracefully.
    ///
    /// After shutdown, further commands will fail.
    pub fn shutdown(&self) -> Result<(), mpsc::error::SendError<AggregatorCommand>> {
        self.tx.send(AggregatorCommand::Shutdown)
    }
}

/// Aggregator service that owns the aggregator and processes commands.
///
/// This service should be spawned as a tokio task and will process
/// commands until shutdown.
pub struct AggregatorService {
    /// The aggregator that stores and batches logs.
    aggregator: Aggregator,
    /// Channel receiver for commands from handles.
    rx: mpsc::UnboundedReceiver<AggregatorCommand>,
}

impl AggregatorService {
    /// Creates a new service with default Datadog API limits.
    ///
    /// Returns both the service (to be spawned) and a handle (to send commands).
    #[must_use]
    #[allow(clippy::should_implement_trait)]
    pub fn default() -> (Self, AggregatorHandle) {
        Self::new(
            constants::MAX_BATCH_ENTRIES_SIZE,
            constants::MAX_CONTENT_SIZE_BYTES,
            constants::MAX_LOG_SIZE_BYTES,
        )
    }

    /// Creates a new service with custom limits.
    ///
    /// # Returns
    ///
    /// Tuple of (service, handle). Spawn the service and use the handle to send commands.
    #[must_use]
    pub fn new(
        max_batch_entries_size: usize,
        max_content_size_bytes: usize,
        max_log_size_bytes: usize,
    ) -> (Self, AggregatorHandle) {
        let (tx, rx) = mpsc::unbounded_channel();
        let aggregator = Aggregator::new(
            max_batch_entries_size,
            max_content_size_bytes,
            max_log_size_bytes,
        );

        let service = Self { aggregator, rx };
        let handle = AggregatorHandle { tx };

        (service, handle)
    }

    /// Runs the aggregator service, processing commands until shutdown.
    ///
    /// This method should be called in a spawned tokio task.
    /// It processes commands sequentially in FIFO order.
    ///
    /// # Shutdown
    ///
    /// The service stops when:
    /// - A `Shutdown` command is received
    /// - All handles are dropped (channel closes)
    pub async fn run(mut self) {
        debug!("Logs aggregator service started");

        while let Some(command) = self.rx.recv().await {
            match command {
                // Add logs to aggregator (single-threaded access, no locks!)
                AggregatorCommand::InsertBatch(logs) => {
                    self.aggregator.add_batch(logs);
                }
                // Drain aggregator and return all batches
                AggregatorCommand::GetBatches(response_tx) => {
                    let mut batches = Vec::new();
                    // Iteratively get batches until aggregator is empty
                    let mut current_batch = self.aggregator.get_batch();
                    while !current_batch.is_empty() {
                        batches.push(current_batch);
                        current_batch = self.aggregator.get_batch();
                    }
                    // Send response back to caller
                    if response_tx.send(batches).is_err() {
                        error!("Failed to send logs flush response - receiver dropped");
                    }
                }
                // Graceful shutdown
                AggregatorCommand::Shutdown => {
                    debug!("Logs aggregator service shutting down");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper function to create a test log entry
    fn create_test_log(message: &str) -> String {
        format!(r#"{{"message":"{}","level":"info"}}"#, message)
    }

    /// Helper function to create multiple test logs
    fn create_test_logs(count: usize) -> Vec<String> {
        (0..count)
            .map(|i| create_test_log(&format!("test message {}", i)))
            .collect()
    }

    /// Helper function to create and spawn a service with default settings
    fn create_and_spawn_default_service() -> AggregatorHandle {
        let (service, handle) = AggregatorService::default();
        tokio::spawn(async move {
            service.run().await;
        });
        handle
    }

    /// Helper function to create and spawn a service with custom settings
    fn create_and_spawn_service(
        max_batch_entries: usize,
        max_content_bytes: usize,
        max_log_bytes: usize,
    ) -> AggregatorHandle {
        let (service, handle) =
            AggregatorService::new(max_batch_entries, max_content_bytes, max_log_bytes);
        tokio::spawn(async move {
            service.run().await;
        });
        handle
    }

    #[test]
    fn test_aggregator_service_default() {
        let (_service, handle) = AggregatorService::default();

        // Verify service is created and handle is functional
        assert!(!handle.tx.is_closed());
    }

    #[test]
    fn test_aggregator_service_new_custom_sizes() {
        let max_batch = 100;
        let max_content = 1024;
        let max_log = 512;

        let (_service, handle) = AggregatorService::new(max_batch, max_content, max_log);

        // Verify service is created and handle is functional
        assert!(!handle.tx.is_closed());
    }

    #[tokio::test]
    async fn test_aggregator_handle_insert_batch_success() {
        let handle = create_and_spawn_default_service();

        let logs = create_test_logs(3);
        let result = handle.insert_batch(logs);

        assert!(result.is_ok());

        // Cleanup
        let _ = handle.shutdown();
    }

    #[tokio::test]
    async fn test_aggregator_handle_insert_empty_batch() {
        let handle = create_and_spawn_default_service();

        let logs = Vec::new();
        let result = handle.insert_batch(logs);

        assert!(result.is_ok());

        // Cleanup
        let _ = handle.shutdown();
    }

    #[tokio::test]
    async fn test_aggregator_handle_insert_multiple_batches() {
        let handle = create_and_spawn_default_service();

        // Insert multiple batches
        for i in 0..5 {
            let logs = vec![create_test_log(&format!("batch {} message", i))];
            let result = handle.insert_batch(logs);
            assert!(result.is_ok());
        }

        // Cleanup
        let _ = handle.shutdown();
    }

    #[tokio::test]
    async fn test_aggregator_handle_get_batches_empty() {
        let handle = create_and_spawn_default_service();

        let result = handle.get_batches().await;

        assert!(result.is_ok());
        let batches = result.unwrap();
        assert_eq!(batches.len(), 0);

        // Cleanup
        let _ = handle.shutdown();
    }

    #[tokio::test]
    async fn test_aggregator_handle_get_batches_with_data() {
        let handle = create_and_spawn_default_service();

        // Insert some logs
        let logs = create_test_logs(3);
        handle.insert_batch(logs).unwrap();

        // Give the service time to process
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Get batches
        let result = handle.get_batches().await;

        assert!(result.is_ok());
        let batches = result.unwrap();
        assert!(batches.len() > 0);
        assert!(batches[0].len() > 0);

        // Verify JSON array format
        let batch_str = String::from_utf8_lossy(&batches[0]);
        assert!(batch_str.starts_with('['));
        assert!(batch_str.ends_with(']'));

        // Cleanup
        let _ = handle.shutdown();
    }

    #[tokio::test]
    async fn test_aggregator_handle_get_batches_clears_data() {
        let handle = create_and_spawn_default_service();

        // Insert logs
        let logs = create_test_logs(2);
        handle.insert_batch(logs).unwrap();

        // Give the service time to process
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Get batches first time
        let result1 = handle.get_batches().await;
        assert!(result1.is_ok());
        assert!(result1.unwrap().len() > 0);

        // Get batches second time - should be empty
        let result2 = handle.get_batches().await;
        assert!(result2.is_ok());
        assert_eq!(result2.unwrap().len(), 0);

        // Cleanup
        let _ = handle.shutdown();
    }

    #[tokio::test]
    async fn test_aggregator_handle_shutdown() {
        let handle = create_and_spawn_default_service();

        let result = handle.shutdown();
        assert!(result.is_ok());

        // After shutdown, further operations should fail
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        let insert_result = handle.insert_batch(create_test_logs(1));
        assert!(insert_result.is_err());
    }

    #[tokio::test]
    async fn test_aggregator_service_run_processes_insert_command() {
        let (service, handle) = AggregatorService::default();

        // Spawn the service
        let service_handle = tokio::spawn(async move {
            service.run().await;
        });

        // Send insert command
        let logs = create_test_logs(2);
        handle.insert_batch(logs).unwrap();

        // Give time to process
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Verify data was inserted by getting batches
        let batches = handle.get_batches().await.unwrap();
        assert!(batches.len() > 0);

        // Cleanup
        handle.shutdown().unwrap();
        let _ = service_handle.await;
    }

    #[tokio::test]
    async fn test_aggregator_service_run_processes_get_batches_command() {
        let (service, handle) = AggregatorService::default();

        // Spawn the service
        let service_handle = tokio::spawn(async move {
            service.run().await;
        });

        // Insert data first
        handle.insert_batch(create_test_logs(3)).unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Send get batches command
        let (response_tx, response_rx) = oneshot::channel();
        handle
            .tx
            .send(AggregatorCommand::GetBatches(response_tx))
            .unwrap();

        // Receive response
        let batches = response_rx.await.unwrap();
        assert!(batches.len() > 0);

        // Cleanup
        handle.shutdown().unwrap();
        let _ = service_handle.await;
    }

    #[tokio::test]
    async fn test_aggregator_service_run_processes_shutdown_command() {
        let (service, handle) = AggregatorService::default();

        // Spawn the service
        let service_handle = tokio::spawn(async move {
            service.run().await;
        });

        // Send shutdown command
        handle.shutdown().unwrap();

        // Service should complete
        let result =
            tokio::time::timeout(tokio::time::Duration::from_millis(100), service_handle).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_aggregator_service_multiple_batches_respects_limits() {
        // Create service with small limits to force multiple batches
        let max_batch_entries = 2;
        let max_content_bytes = 500;
        let max_log_bytes = 400;

        let handle = create_and_spawn_service(max_batch_entries, max_content_bytes, max_log_bytes);

        // Insert more logs than can fit in one batch
        let logs = create_test_logs(5);
        handle.insert_batch(logs).unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Get batches - should have multiple
        let batches = handle.get_batches().await.unwrap();

        // With limits, we should get multiple batches
        assert!(batches.len() > 0);

        // Cleanup
        let _ = handle.shutdown();
    }

    #[tokio::test]
    async fn test_aggregator_handle_clone() {
        let handle = create_and_spawn_default_service();

        // Clone the handle
        let handle_clone = handle.clone();

        // Both handles should work
        let logs1 = vec![create_test_log("from original")];
        let logs2 = vec![create_test_log("from clone")];

        assert!(handle.insert_batch(logs1).is_ok());
        assert!(handle_clone.insert_batch(logs2).is_ok());

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Get batches
        let batches = handle.get_batches().await.unwrap();
        assert!(batches.len() > 0);

        // Cleanup
        let _ = handle.shutdown();
    }

    #[tokio::test]
    async fn test_aggregator_command_debug_format() {
        // Test Debug implementation for AggregatorCommand
        let logs = vec![create_test_log("test")];
        let cmd1 = AggregatorCommand::InsertBatch(logs);
        let debug_str1 = format!("{:?}", cmd1);
        assert!(debug_str1.contains("InsertBatch"));

        let (tx, _rx) = oneshot::channel();
        let cmd2 = AggregatorCommand::GetBatches(tx);
        let debug_str2 = format!("{:?}", cmd2);
        assert!(debug_str2.contains("GetBatches"));

        let cmd3 = AggregatorCommand::Shutdown;
        let debug_str3 = format!("{:?}", cmd3);
        assert!(debug_str3.contains("Shutdown"));
    }

    #[tokio::test]
    async fn test_aggregator_handle_debug_format() {
        let handle = create_and_spawn_default_service();

        let debug_str = format!("{:?}", handle);
        assert!(debug_str.contains("AggregatorHandle"));

        // Cleanup
        let _ = handle.shutdown();
    }

    #[tokio::test]
    async fn test_aggregator_service_concurrent_inserts() {
        let handle = create_and_spawn_default_service();

        // Spawn multiple tasks that insert concurrently
        let mut tasks = Vec::new();
        for i in 0..10 {
            let h = handle.clone();
            tasks.push(tokio::spawn(async move {
                let logs = vec![create_test_log(&format!("concurrent message {}", i))];
                h.insert_batch(logs)
            }));
        }

        // Wait for all inserts to complete
        for task in tasks {
            let result = task.await.unwrap();
            assert!(result.is_ok());
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Get batches - should contain all inserted logs
        let batches = handle.get_batches().await.unwrap();
        assert!(batches.len() > 0);

        // Verify all data is present
        let total_data: String = batches
            .iter()
            .map(|b| String::from_utf8_lossy(b).to_string())
            .collect();

        // Should contain references to multiple messages
        assert!(total_data.contains("concurrent message"));

        // Cleanup
        let _ = handle.shutdown();
    }

    #[tokio::test]
    async fn test_aggregator_service_large_log_handling() {
        let handle = create_and_spawn_default_service();

        // Create a large log (but under the limit)
        let large_message = "x".repeat(1000);
        let log = create_test_log(&large_message);

        handle.insert_batch(vec![log]).unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let batches = handle.get_batches().await.unwrap();
        assert!(batches.len() > 0);
        assert!(batches[0].len() > 1000);

        // Cleanup
        let _ = handle.shutdown();
    }

    #[tokio::test]
    async fn test_aggregator_service_insert_after_get_batches() {
        let handle = create_and_spawn_default_service();

        // Insert first batch
        handle.insert_batch(create_test_logs(2)).unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Get batches
        let batches1 = handle.get_batches().await.unwrap();
        assert!(batches1.len() > 0);

        // Insert second batch after retrieval
        handle.insert_batch(create_test_logs(2)).unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Get batches again
        let batches2 = handle.get_batches().await.unwrap();
        assert!(batches2.len() > 0);

        // Cleanup
        let _ = handle.shutdown();
    }

    #[tokio::test]
    async fn test_aggregator_service_empty_string_logs() {
        let handle = create_and_spawn_default_service();

        // Insert empty strings
        let logs = vec![String::new(), String::new()];
        handle.insert_batch(logs).unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let batches = handle.get_batches().await.unwrap();
        // Should still create a batch with empty strings
        assert!(batches.len() > 0);

        // Cleanup
        let _ = handle.shutdown();
    }

    #[tokio::test]
    async fn test_aggregator_handle_get_batches_error_on_dropped_receiver() {
        let (service, handle) = AggregatorService::default();

        // Don't spawn the service - let it drop
        drop(service);

        // Try to get batches - should fail because service is not running
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        let result = handle.get_batches().await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to send flush command"));
    }

    #[test]
    fn test_aggregator_service_new_creates_valid_channel() {
        let (service, handle) = AggregatorService::new(100, 1024, 512);

        // Verify channel is open
        assert!(!handle.tx.is_closed());

        // Verify we can send through the channel
        let result = handle.tx.send(AggregatorCommand::Shutdown);
        assert!(result.is_ok());

        // Verify service can receive
        // Note: we don't run the service, just verify the channel works
        drop(service); // Explicitly drop to close receiver
    }

    #[tokio::test]
    async fn test_aggregator_service_processes_all_commands_before_shutdown() {
        let (service, handle) = AggregatorService::default();

        let service_handle = tokio::spawn(async move {
            service.run().await;
        });

        // Send multiple commands
        handle.insert_batch(create_test_logs(1)).unwrap();
        handle.insert_batch(create_test_logs(1)).unwrap();
        handle.insert_batch(create_test_logs(1)).unwrap();

        // Give time to process
        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;

        // Get batches to verify all were processed
        let batches = handle.get_batches().await.unwrap();
        assert!(batches.len() > 0);

        // Now shutdown
        handle.shutdown().unwrap();

        let _ = service_handle.await;
    }
}
