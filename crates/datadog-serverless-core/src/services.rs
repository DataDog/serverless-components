use crate::{config::ServicesConfig, error::ServicesError};
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

/// Status of the serverless services.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceStatus {
    /// Services are starting up.
    Starting,
    /// Services are running normally.
    Running,
    /// Services are shutting down.
    Stopping,
    /// Services have stopped.
    Stopped,
}

/// Handle to the running serverless services.
///
/// This handle allows checking the status and stopping the services.
#[derive(Debug, Clone)]
pub struct ServicesHandle {
    status: Arc<RwLock<ServiceStatus>>,
    status_tx: broadcast::Sender<ServiceStatus>,
    shutdown_tx: broadcast::Sender<()>,
}

impl ServicesHandle {
    /// Check if the services are currently running.
    pub async fn is_running(&self) -> bool {
        matches!(*self.status.read().await, ServiceStatus::Running)
    }

    /// Get a receiver for status updates.
    pub fn status_receiver(&self) -> broadcast::Receiver<ServiceStatus> {
        self.status_tx.subscribe()
    }

    /// Stop the services.
    pub async fn stop(&self) -> Result<(), ServicesError> {
        let mut status = self.status.write().await;
        if *status == ServiceStatus::Stopped {
            return Ok(());
        }

        *status = ServiceStatus::Stopping;
        drop(status);

        // Signal shutdown
        let _ = self.shutdown_tx.send(());

        Ok(())
    }
}

/// Main serverless services coordinator.
///
/// This struct manages the lifecycle of both the trace agent and DogStatsD services.
#[derive(Debug)]
pub struct ServerlessServices {
    config: ServicesConfig,
}

impl ServerlessServices {
    /// Create a new ServerlessServices instance.
    pub fn new(config: ServicesConfig) -> Self {
        Self { config }
    }

    /// Start the serverless services.
    ///
    /// This will start both the trace agent and DogStatsD services based on the
    /// configuration. Returns a handle that can be used to monitor and control
    /// the services.
    pub async fn start(self) -> Result<ServicesHandle, ServicesError> {
        let status = Arc::new(RwLock::new(ServiceStatus::Starting));
        let (status_tx, _status_rx) = broadcast::channel(16);
        let (shutdown_tx, shutdown_rx) = broadcast::channel(16);

        let handle = ServicesHandle {
            status: Arc::clone(&status),
            status_tx: status_tx.clone(),
            shutdown_tx,
        };

        // Spawn the main services task
        let status_clone = Arc::clone(&status);
        tokio::spawn(async move {
            if let Err(e) =
                Self::run_services(self.config, status_clone, shutdown_rx, status_tx).await
            {
                tracing::error!("Services error: {}", e);
            }
        });

        // Wait for services to reach Running state
        let mut timeout = tokio::time::interval(std::time::Duration::from_millis(100));
        for _ in 0..50 {
            timeout.tick().await;
            if *status.read().await == ServiceStatus::Running {
                break;
            }
        }

        Ok(handle)
    }

    async fn run_services(
        _config: ServicesConfig,
        status: Arc<RwLock<ServiceStatus>>,
        mut shutdown_rx: broadcast::Receiver<()>,
        status_tx: broadcast::Sender<ServiceStatus>,
    ) -> Result<(), ServicesError> {
        // Update status to Running
        {
            let mut s = status.write().await;
            *s = ServiceStatus::Running;
            let _ = status_tx.send(ServiceStatus::Running);
        }

        // TODO: Start actual services here (trace agent, dogstatsd)
        // For now, just wait for shutdown signal
        let _ = shutdown_rx.recv().await;

        // Update status to Stopped
        {
            let mut s = status.write().await;
            *s = ServiceStatus::Stopped;
            let _ = status_tx.send(ServiceStatus::Stopped);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_services_start() {
        let config = ServicesConfig::default();
        let services = ServerlessServices::new(config);
        let handle = services.start().await.unwrap();

        assert!(handle.is_running().await);
        handle.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_services_stop() {
        let config = ServicesConfig::default();
        let services = ServerlessServices::new(config);
        let handle = services.start().await.unwrap();

        handle.stop().await.unwrap();

        // Wait a bit for the stop to complete
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let status = *handle.status.read().await;
        assert_eq!(status, ServiceStatus::Stopped);
    }

    #[tokio::test]
    async fn test_services_stop_idempotent() {
        let config = ServicesConfig::default();
        let services = ServerlessServices::new(config);
        let handle = services.start().await.unwrap();

        handle.stop().await.unwrap();
        handle.stop().await.unwrap(); // Second stop should be fine

        // Wait a bit for the stop to complete
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let status = *handle.status.read().await;
        assert_eq!(status, ServiceStatus::Stopped);
    }

    #[tokio::test]
    async fn test_services_status_receiver() {
        let config = ServicesConfig::default();
        let services = ServerlessServices::new(config);
        let handle = services.start().await.unwrap();

        let mut rx = handle.status_receiver();
        handle.stop().await.unwrap();

        // Should receive status update
        tokio::time::timeout(tokio::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn test_service_status_transitions() {
        let config = ServicesConfig::default();
        let services = ServerlessServices::new(config);
        let handle = services.start().await.unwrap();

        // Should be running
        assert!(handle.is_running().await);

        handle.stop().await.unwrap();

        // Wait for stop to complete
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Should no longer be running
        assert!(!handle.is_running().await);
    }
}
