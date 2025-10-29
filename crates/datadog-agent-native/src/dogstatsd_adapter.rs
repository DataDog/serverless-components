//! DogStatsD adapter for datadog-agent-native
//!
//! This module provides a wrapper around the external `dogstatsd` crate to add:
//! - Ephemeral port support (port 0)
//! - Returning the actual bound port
//! - Future extensibility without modifying the shared dogstatsd crate

use anyhow::Context;
use dogstatsd::aggregator_service::AggregatorHandle;
use dogstatsd::dogstatsd::{DogStatsD, DogStatsDConfig};
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;

/// Adapter for DogStatsD that adds ephemeral port support
pub struct DogStatsDAdapter {
    inner: DogStatsD,
    actual_port: u16,
}

impl DogStatsDAdapter {
    const MAX_BIND_ATTEMPTS: usize = 5;

    /// Create a new DogStatsD instance with ephemeral port support
    ///
    /// If config.port is 0, an ephemeral port will be allocated.
    /// Returns the DogStatsD instance and the actual bound port.
    pub async fn new(
        config: &DogStatsDConfig,
        aggregator_handle: AggregatorHandle,
        cancel_token: CancellationToken,
    ) -> anyhow::Result<(Self, u16)> {
        if config.port == 0 {
            // Attempt to bind DogStatsD on an ephemeral port. We retry a few times to
            // avoid races where another process grabs the port between discovery and bind.
            let mut last_panic: Option<String> = None;
            for attempt in 1..=Self::MAX_BIND_ATTEMPTS {
                let addr = format!("{}:0", config.host);
                let socket = tokio::net::UdpSocket::bind(&addr).await.with_context(|| {
                    format!("Failed to bind ephemeral DogStatsD socket on attempt {attempt}")
                })?;
                let actual_port = socket
                    .local_addr()
                    .with_context(|| {
                        format!(
                            "Failed to read local address for DogStatsD socket on attempt {attempt}"
                        )
                    })?
                    .port();
                drop(socket);

                let actual_config = DogStatsDConfig {
                    host: config.host.clone(),
                    port: actual_port,
                };

                let aggregator_handle_clone = aggregator_handle.clone();
                let cancel_token_clone = cancel_token.clone();

                let join_handle = tokio::spawn(async move {
                    DogStatsD::new(&actual_config, aggregator_handle_clone, cancel_token_clone)
                        .await
                });

                match join_handle.await {
                    Ok(inner) => {
                        return Ok((Self { inner, actual_port }, actual_port));
                    }
                    Err(join_err) => {
                        if join_err.is_panic() {
                            last_panic = Some(format_panic(join_err));
                            continue;
                        }
                        if join_err.is_cancelled() {
                            anyhow::bail!(
                                "DogStatsD startup task cancelled while binding to ephemeral port"
                            );
                        }
                        return Err(anyhow::anyhow!("DogStatsD startup task failed: {join_err}"));
                    }
                }
            }

            let reason = last_panic.unwrap_or_else(|| "unknown panic".to_string());
            anyhow::bail!(
                "Failed to start DogStatsD on an ephemeral port after {} attempts: {reason}",
                Self::MAX_BIND_ATTEMPTS
            );
        }

        // Non-ephemeral port: start once and surface any panic as an error.
        let config_clone = DogStatsDConfig {
            host: config.host.clone(),
            port: config.port,
        };
        let cancel_token_clone = cancel_token.clone();
        let join_handle = tokio::spawn(async move {
            DogStatsD::new(&config_clone, aggregator_handle, cancel_token_clone).await
        });

        let inner = match join_handle.await {
            Ok(inner) => inner,
            Err(join_err) => {
                if join_err.is_panic() {
                    let message = format_panic(join_err);
                    anyhow::bail!("DogStatsD startup panic: {message}");
                }
                if join_err.is_cancelled() {
                    anyhow::bail!("DogStatsD startup task cancelled before completion");
                }
                return Err(anyhow::anyhow!("DogStatsD startup task failed: {join_err}"));
            }
        };

        Ok((
            Self {
                inner,
                actual_port: config.port,
            },
            config.port,
        ))
    }

    /// Get the actual bound port
    pub fn actual_port(&self) -> u16 {
        self.actual_port
    }

    /// Start the DogStatsD server
    pub async fn spin(self) {
        self.inner.spin().await;
    }
}

fn format_panic(join_err: JoinError) -> String {
    match join_err.into_panic().downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => match payload.downcast::<&'static str>() {
            Ok(message) => (*message).to_string(),
            Err(_) => "unknown panic".to_string(),
        },
    }
}
