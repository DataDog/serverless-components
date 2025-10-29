//! Websocket echo task management.
//!
//! The Go agent periodically performs a websocket echo exchange to confirm that
//! remote-config websocket infrastructure remains healthy.  This module mirrors
//! that behaviour so the scheduler can delegate the detailed logic elsewhere.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;
use tokio::time::{timeout, Duration, Instant};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Message as WsMessage},
};
use tracing::{debug, warn};

use super::refresh::ServiceShared;
use super::state::WebsocketCheckResult;

const ECHO_TEST_PATH: &str = "/api/v0.2/echo-test";
const MAX_ECHO_MESSAGE_SIZE: usize = 50 * 1024 * 1024; // 50 MiB

#[cfg(not(test))]
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
#[cfg(test)]
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(1);

impl ServiceShared {
    /// Runs the websocket echo loop until shutdown or configuration disables it.
    pub(crate) async fn run_websocket_echo(
        self: Arc<Self>,
        shutdown_rx: &mut broadcast::Receiver<()>,
    ) {
        if self.config.websocket_echo_interval.is_zero() {
            return;
        }

        let mut ticker = tokio::time::interval(self.config.websocket_echo_interval);
        let mut first = true;
        loop {
            if first {
                first = false;
                if shutdown_rx.try_recv().is_ok() {
                    break;
                }
            } else {
                tokio::select! {
                    _ = shutdown_rx.recv() => break,
                    _ = ticker.tick() => {}
                }
            }

            // Check for shutdown before starting websocket connection
            if shutdown_rx.try_recv().is_ok() {
                break;
            }

            let (result, error) = self.perform_websocket_echo_with_shutdown(shutdown_rx).await;
            self.record_websocket_check(result, error).await;
        }
    }

    /// Executes a single websocket echo exchange with shutdown signal monitoring.
    pub(crate) async fn perform_websocket_echo_with_shutdown(
        &self,
        shutdown_rx: &mut broadcast::Receiver<()>,
    ) -> (WebsocketCheckResult, Option<String>) {
        let session_fut = self.websocket_echo_session();

        tokio::select! {
            _ = shutdown_rx.recv() => {
                // Shutdown signal received, abort websocket operation immediately
                (WebsocketCheckResult::Failure, Some("shutdown requested".to_string()))
            }
            result = session_fut => {
                match result {
                    Ok(_) => (WebsocketCheckResult::Success, None),
                    Err(err) => {
                        warn!("remote-config websocket echo test failed: {err}");
                        (WebsocketCheckResult::Failure, Some(err))
                    }
                }
            }
        }
    }

    /// Establishes the websocket connection and mirrors payloads back to the peer.
    pub(crate) async fn websocket_echo_session(&self) -> Result<(), String> {
        let request = self
            .http_client
            .websocket_request(ECHO_TEST_PATH)
            .await
            .map_err(|err| err.to_string())?;

        let mut config = WebSocketConfig::default();
        config.max_message_size = Some(MAX_ECHO_MESSAGE_SIZE);
        config.max_frame_size = Some(MAX_ECHO_MESSAGE_SIZE);

        let timeout_duration = self.config.websocket_echo_timeout;
        let fut = async {
            let (mut stream, _) = connect_async_with_config(request, Some(config), false)
                .await
                .map_err(|err| err.to_string())?;
            let mut compression_enabled = false;
            loop {
                let frame = timeout(MESSAGE_TIMEOUT, stream.next()).await.map_err(|_| {
                    format!(
                        "websocket echo timed out after {:?} without receiving a frame",
                        MESSAGE_TIMEOUT
                    )
                })?;
                match frame {
                    Some(Ok(WsMessage::Text(text))) => {
                        // Text frames may carry compression commands and must be echoed verbatim.
                        if let Some(enabled) = compression_command(&text) {
                            update_compression_state(enabled, &mut compression_enabled);
                        }
                        stream
                            .send(WsMessage::Text(text))
                            .await
                            .map_err(|err| err.to_string())?;
                    }
                    Some(Ok(WsMessage::Binary(bytes))) => {
                        // Binary frames are mirrored byte-for-byte to satisfy the echo contract.
                        stream
                            .send(WsMessage::Binary(bytes))
                            .await
                            .map_err(|err| err.to_string())?;
                    }
                    Some(Ok(WsMessage::Ping(payload))) => {
                        // Ping frames require immediate pong responses to keep the session alive.
                        stream
                            .send(WsMessage::Pong(payload))
                            .await
                            .map_err(|err| err.to_string())?;
                    }
                    Some(Ok(WsMessage::Pong(_))) => {
                        // Pong frames merely confirm the peer observed our ping.
                    }
                    Some(Ok(WsMessage::Frame(_))) => {
                        // Non-text/binary frames are outside the echo protocol we implement.
                        return Err("unsupported websocket frame received".into());
                    }
                    Some(Ok(WsMessage::Close(_))) | None => {
                        // Close frames (or EOF) signal clean termination.
                        return Ok(());
                    }
                    Some(Err(err)) => {
                        // Transport-layer failures bubble up for telemetry/error handling.
                        return Err(err.to_string());
                    }
                }
            }
        };

        match timeout(timeout_duration, fut).await {
            Ok(result) => {
                // Session finished before the timeout elapsed.
                result
            }
            Err(_) => {
                // Timeout branch indicates the server stayed idle for too long.
                Err(format!(
                    "websocket echo timed out after {:?}",
                    timeout_duration
                ))
            }
        }
    }

    /// Stores the most recent websocket check outcome and emits telemetry.
    pub(crate) async fn record_websocket_check(
        &self,
        result: WebsocketCheckResult,
        error: Option<String>,
    ) {
        {
            let mut guard = self.state.lock().await;
            guard.last_websocket_result = Some(result);
            guard.last_websocket_check = Some(Instant::now());
            guard.last_websocket_error = error;
        }
        let telemetry = self.telemetry.read().await.clone();
        telemetry.on_websocket_check(result);
    }
}

/// Returns whether the backend payload requests compression toggling.
fn compression_command(payload: &str) -> Option<bool> {
    match payload {
        "set_compress_on" => {
            // Backend explicitly enables write compression mid-session.
            Some(true)
        }
        "set_compress_off" => {
            // Backend disables write compression and expects plain frames.
            Some(false)
        }
        _ => {
            // All other payloads are normal echo messages with no side effects.
            None
        }
    }
}

/// Records compression toggles issued by the backend.
///
/// tokio-tungstenite does not expose APIs to change the compression setting
/// after the handshake completes (Go uses `EnableWriteCompression`).  We log
/// the intent so future transport upgrades can act on it while keeping the
/// behaviour visible for parity investigations.
fn update_compression_state(next: bool, current: &mut bool) {
    if *current == next {
        // Skip updates when the requested state already matches the local state.
        return;
    }
    *current = next;
    debug!(
        enabled = next,
        "remote-config websocket toggled write compression"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::test_support::{base_config, build_service_with_base_url};
    use tokio::time::Duration;

    /// Ensures websocket sessions report failures when the endpoint is unreachable.
    #[tokio::test(flavor = "multi_thread")]
    async fn websocket_echo_session_fails_on_unreachable_endpoint() {
        let mut config = base_config();
        config.enable_websocket_echo = true;
        config.disable_background_poller = true;
        config.org_status_interval = Duration::from_secs(300);
        config.websocket_echo_interval = Duration::from_secs(60);
        config.websocket_echo_timeout = Duration::from_millis(100);

        let service = build_service_with_base_url("http://127.0.0.1:9".into(), config);
        let result = service.shared_for_tests().websocket_echo_session().await;
        assert!(
            result.is_err(),
            "expected websocket error for unreachable endpoint"
        );
    }

    /// Verifies compression command parsing toggles the right states.
    #[test]
    fn compression_command_recognizes_toggles() {
        assert_eq!(compression_command("set_compress_on"), Some(true));
        assert_eq!(compression_command("set_compress_off"), Some(false));
        assert_eq!(compression_command("noop"), None);
    }

    /// Ensures the compression state helper updates only when necessary.
    #[test]
    fn update_compression_state_toggles_flag() {
        let mut state = false;
        update_compression_state(true, &mut state);
        assert!(state, "state should enable compression");

        // Re-applying the same state should leave the flag untouched.
        update_compression_state(true, &mut state);
        assert!(state, "state should remain enabled");

        update_compression_state(false, &mut state);
        assert!(!state, "state should disable compression");
    }
}
