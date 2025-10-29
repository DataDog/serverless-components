//! Refresh loop orchestration.
//!
//! This module hosts the shared service internals responsible for polling the
//! backend, updating the Uptane store, handling cache-bypass requests, and
//! driving telemetry notifications.  Higher-level APIs in `core.rs` delegate
//! to these helpers.

use std::sync::{Arc, RwLock as StdRwLock};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{debug, error, warn};

use super::config::ServiceConfig;
use super::state::{RefreshErrorLevel, RuntimeMetadata, ServiceState};
use super::telemetry::RemoteConfigTelemetry;
use super::{CacheBypassSignal, RefreshOutcome, ServiceError};
use crate::bootstrap::HostRuntimeSnapshot;
use crate::embedded_roots;
use crate::http::HttpClient;
use crate::rc_key::RcKey;
use crate::store::StoreError;
use crate::uptane::{TufVersions, UptaneError, UptaneState};
use remote_config_proto::remoteconfig::LatestConfigsResponse;

const MIN_REQUEST_SPACING_SECONDS: u64 = 1;
const MAX_IMMEDIATE_RETRIES: u32 = 3;

/// Shared service internals utilised by background tasks and API calls.
pub(crate) struct ServiceShared {
    /// HTTP client responsible for communicating with the backend.
    pub(crate) http_client: HttpClient,
    /// Uptane state manager that persists metadata and target files.
    pub(crate) uptane: UptaneState,
    /// Mutable state guarded by a mutex.
    pub(super) state: Mutex<ServiceState>,
    /// Sender used to request cache-bypass refreshes.
    pub(crate) cache_bypass_tx: Mutex<Option<mpsc::Sender<CacheBypassSignal>>>,
    /// Telemetry sink used to report service metrics.
    pub(crate) telemetry: RwLock<Arc<dyn RemoteConfigTelemetry>>,
    /// Runtime metadata describing hostname/tags/trace env.
    pub(crate) runtime: RwLock<RuntimeMetadata>,
    /// Static runtime configuration.
    pub(crate) config: ServiceConfig,
    /// RC key currently enforced for org binding (updated on credential rotation).
    pub(crate) rc_key: StdRwLock<Option<RcKey>>,
    /// Status handle exposing org flags and last error.
    pub(crate) status: Arc<crate::status::RemoteConfigStatus>,
    /// Join handles for auxiliary background tasks (runtime/credential watchers).
    pub(crate) aux_tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl std::fmt::Debug for ServiceShared {
    /// Keeps debug output concise by only printing static config details.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceShared")
            .field("config", &self.config)
            .finish()
    }
}

impl ServiceShared {
    /// Wipes the Uptane cache and re-seeds embedded trust roots while rebuilding in-memory state.
    pub(crate) async fn reset_state(&self) -> Result<(), ServiceError> {
        self.uptane.reset_cache()?;
        if let Some(api_key) = self.http_client.api_key().await {
            self.uptane
                .update_metadata(
                    &self.config.agent_version,
                    api_key.as_str(),
                    self.http_client.base_url(),
                )
                .map_err(ServiceError::Uptane)?;
        } else {
            warn!(
                "remote-config: API key unavailable during cache reset; leaving metadata unchanged"
            );
        }
        let (config_root, director_root) = embedded_roots::resolve(
            &self.config.site,
            self.config.config_root_override.as_deref(),
            self.config.director_root_override.as_deref(),
        )
        .map_err(|err| ServiceError::EmbeddedRoot(err.to_string()))?;
        self.uptane
            .seed_trust_roots(&config_root.raw, &director_root.raw)
            .map_err(ServiceError::Uptane)?;
        {
            let mut guard = self.state.lock().await;
            *guard = ServiceState::new(&self.config);
        }
        Ok(())
    }

    /// Returns the current runtime metadata snapshot used for refresh requests.
    pub(crate) async fn runtime_metadata(&self) -> RuntimeMetadata {
        self.runtime.read().await.clone()
    }

    /// Merges a host-provided runtime snapshot into the current metadata.
    pub(crate) async fn apply_runtime_snapshot(&self, snapshot: HostRuntimeSnapshot) {
        let mut guard = self.runtime.write().await;
        guard.merge_snapshot(&snapshot, &self.config);
    }

    /// Registers a background task whose lifetime should align with the service.
    pub(crate) async fn register_aux_task(&self, handle: JoinHandle<()>) {
        let mut guard = self.aux_tasks.lock().await;
        guard.push(handle);
    }

    /// Aborts every auxiliary task, awaiting their shutdown to drop Arc references.
    pub(crate) async fn abort_aux_tasks(&self) {
        let mut guard = self.aux_tasks.lock().await;
        for handle in guard.drain(..) {
            handle.abort();
            let _ = handle.await;
        }
    }

    /// Runs the configuration poller loop responsible for periodic refreshes.
    pub(crate) async fn run_config_poller(
        self: Arc<Self>,
        mut bypass_rx: mpsc::Receiver<CacheBypassSignal>,
        shutdown_rx: &mut broadcast::Receiver<()>,
    ) {
        let mut next_delay = Duration::from_secs(0);
        let mut immediate_retry_streak = 0u32;
        loop {
            tokio::select! {
                biased;
                // Shutdown signal terminates the poller immediately, just like the Go agent.
                _ = shutdown_rx.recv() => {
                    debug!("remote-config config poller shutting down");
                    break;
                }
                // Cache-bypass requests trigger an immediate refresh and acknowledgement to the caller.
                Some(signal) = bypass_rx.recv() => {
                    debug!("remote-config bypass signal received");
                    let result = self.perform_refresh().await;
                    match result {
                        Ok(outcome) => {
                            next_delay = outcome.next_interval;
                            immediate_retry_streak = 0;
                            self.notify_bypass(signal.completion, Ok(()));
                        }
                        Err(err) => {
                            let message = err.to_string();
                            let immediate_retry = should_retry_immediately(&err);
                            let mut delay = self.handle_refresh_error(&err).await;
                            apply_immediate_retry_policy(
                                &mut immediate_retry_streak,
                                &err,
                                immediate_retry,
                                "bypass",
                                &mut delay,
                            );
                            next_delay = delay;
                            self.notify_bypass(signal.completion, Err(message));
                        }
                    }
                }
                // Absent bypass signals, fall back to the periodic timer-driven refresh cadence.
                _ = sleep(next_delay) => {
                    let result = self.perform_refresh().await;
                    match result {
                        Ok(outcome) => {
                            next_delay = outcome.next_interval;
                            immediate_retry_streak = 0;
                        }
                        Err(err) => {
                            let immediate_retry = should_retry_immediately(&err);
                            let mut delay = self.handle_refresh_error(&err).await;
                            apply_immediate_retry_policy(
                                &mut immediate_retry_streak,
                                &err,
                                immediate_retry,
                                "poller",
                                &mut delay,
                            );
                            next_delay = delay;
                        }
                    }
                }
            }
        }
    }

    /// Processes bypass-triggered refreshes when the background poller is disabled.
    ///
    /// Mirrors the Go agent `startWithoutAgentPollLoop`: the service waits for
    /// cache-bypass requests, performs a refresh immediately, and notifies the
    /// requester once the outcome is known.
    pub(crate) async fn run_on_demand_poller(
        self: Arc<Self>,
        mut bypass_rx: mpsc::Receiver<CacheBypassSignal>,
        shutdown_rx: &mut broadcast::Receiver<()>,
    ) {
        loop {
            tokio::select! {
                // Shutdown cancels the on-demand loop immediately; remaining bypass requests drop.
                _ = shutdown_rx.recv() => {
                    // Shutdown signal observed; exit without draining pending bypass signals.
                    debug!("remote-config on-demand poller shutting down");
                    break;
                }
                maybe_signal = bypass_rx.recv() => {
                    match maybe_signal {
                        // Each bypass request is handled synchronously to keep semantics close to Go.
                        Some(signal) => {
                            // Each signal represents a single refresh request issued by a new client.
                            match self.perform_refresh().await {
                                Ok(_) => {
                                    // Refresh succeeded; inform the waiting client that new data is available.
                                    self.notify_bypass(signal.completion, Ok(()));
                                }
                                Err(err) => {
                                    // Refresh failed; propagate the error string and let the backoff logic record it.
                                    let immediate_retry = should_retry_immediately(&err);
                                    self.handle_refresh_error(&err).await;
                                    self.notify_bypass(signal.completion, Err(err.to_string()));
                                    if immediate_retry {
                                        // Manual mode has no timer loop, so issue bounded follow-up attempts immediately.
                                        for retry in 1..=MAX_IMMEDIATE_RETRIES {
                                            debug!(
                                                %err,
                                                retry_count = retry,
                                                "remote-config on-demand poller retrying immediately after payload failure"
                                            );
                                            match self.perform_refresh().await {
                                                Ok(_) => break,
                                                Err(next_err) => {
                                                    let continue_retry = should_retry_immediately(&next_err);
                                                    self.handle_refresh_error(&next_err).await;
                                                    if !continue_retry {
                                                        break;
                                                    }
                                                    if retry == MAX_IMMEDIATE_RETRIES {
                                                        debug!(
                                                            %next_err,
                                                            max_retries = MAX_IMMEDIATE_RETRIES,
                                                            "remote-config on-demand poller exhausted immediate retry budget"
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        // Channel closure indicates no more clients need servicing; exit loop.
                        None => {
                            // All senders dropped the channel, so there is nothing else to service.
                            debug!("remote-config on-demand poller detected closed bypass channel");
                            break;
                        }
                    }
                }
            }
        }
    }

    /// Performs a refresh attempt and returns the resulting next interval.
    ///
    /// Handles timestamp guards, spacing constraints, Uptane updates, and telemetry.
    pub(crate) async fn perform_refresh(&self) -> Result<RefreshOutcome, ServiceError> {
        debug!("remote-config: Starting refresh cycle");

        // Branch handles timestamp jitter: we stay in this loop until the guard reports we are
        // safely beyond the previous attempt's one-second resolution boundary.
        loop {
            let wait_boundary = {
                let guard = self.state.lock().await;
                guard.time_until_timestamp_boundary(Duration::from_secs(1))
            };
            if let Some(delay) = wait_boundary {
                // Timestamp collision detected; delay the refresh so our log/telemetry timestamps
                // remain monotonic and match the Go agent semantics.
                debug!(
                    "remote-config delaying refresh by {:?} to respect timestamp resolution",
                    delay
                );
                sleep(delay).await;
            } else {
                break;
            }
        }

        // Enforce the minimum spacing between requests so bursty cache-bypass activity does not
        // overwhelm the backend.
        if let Some(wait) = self
            .state
            .lock()
            .await
            .time_until_next_attempt(Duration::from_secs(MIN_REQUEST_SPACING_SECONDS))
        {
            debug!(
                "remote-config throttling refresh attempt for {} ms",
                wait.as_millis()
            );
            sleep(wait).await;
        }

        {
            let mut guard = self.state.lock().await;
            guard.record_attempt();
        }

        // Determine which TUF metadata versions to advertise in the next request; each branch
        // below maps to a specific store state.
        let versions = match self.uptane.tuf_versions() {
            Ok(versions) => {
                // Happy path: cached metadata exists, so report the precise director/config state.
                debug!(
                    "remote-config: Current TUF versions - director_root: {}, director_targets: {}, config_root: {}, config_snapshot: {}",
                    versions.director_root, versions.director_targets, versions.config_root, versions.config_snapshot
                );
                versions
            }
            Err(UptaneError::Store(StoreError::MissingMetadata)) => {
                // Fresh cache: fall back to the zeroed versions so the backend returns full roots.
                debug!("remote-config: No existing TUF metadata found, starting from scratch");
                TufVersions::default()
            }
            Err(err) => return Err(ServiceError::Uptane(err)),
        };
        let backend_state = self.backend_client_state()?;
        self.ensure_org_uuid().await?;
        let runtime = self.runtime_metadata().await;
        let request = {
            let mut guard = self.state.lock().await;
            guard.build_latest_configs_request(&self.config, &runtime, versions, backend_state)
        };

        debug!("remote-config: Fetching latest configs from backend");
        let response = self.http_client.fetch_latest_configs(&request).await?;

        // Log response details
        let num_target_files = response.target_files.len();
        let has_config_metas = response.config_metas.is_some();
        let has_director_metas = response.director_metas.is_some();
        debug!(
            "remote-config: Received response - target_files: {}, has_config_metas: {}, has_director_metas: {}",
            num_target_files, has_config_metas, has_director_metas
        );

        if num_target_files == 0 {
            // Backend responded without target files; keep logging loud so operators can diagnose.
            debug!("remote-config: WARNING - Backend returned 0 target_files!");
        } else {
            debug!(
                "remote-config: Backend returned {} target_files",
                num_target_files
            );
            for (idx, file) in response.target_files.iter().enumerate() {
                debug!(
                    "remote-config:   target_file[{}]: path='{}', size={} bytes",
                    idx,
                    file.path,
                    file.raw.len()
                );
            }
        }

        debug!("remote-config: Updating local Uptane store with response");
        self.uptane.update(&response)?;

        let override_interval = extract_refresh_override(&response);

        let mut guard = self.state.lock().await;
        guard.clear_cached_paths();
        let next = guard.handle_refresh_success(&self.config, override_interval);
        debug!(
            "remote-config refresh succeeded, scheduling next refresh in {} ms",
            next.as_millis()
        );
        drop(guard);
        self.record_refresh_success(next).await;
        Ok(RefreshOutcome {
            next_interval: next,
        })
    }

    /// Notifies cache-bypass waiters about the outcome of a refresh.
    /// Notifies the requester waiting on a bypass completion channel.
    pub(crate) fn notify_bypass(
        &self,
        completion: Option<oneshot::Sender<Result<(), String>>>,
        result: Result<(), String>,
    ) {
        if let Some(tx) = completion {
            let _ = tx.send(result);
        }
    }

    /// Records refresh failures and returns the next delay derived from backoff.
    pub(crate) async fn handle_refresh_error(&self, error: &ServiceError) -> Duration {
        let (delay, level) = {
            let mut guard = self.state.lock().await;
            guard.handle_refresh_error(&self.config, error)
        };

        match level {
            RefreshErrorLevel::Debug => {
                debug!(%error, "remote-config refresh failed");
            }
            RefreshErrorLevel::Warn => {
                warn!(%error, "remote-config refresh failed");
            }
            RefreshErrorLevel::Error => {
                error!(%error, "remote-config refresh failed");
            }
        }

        self.record_refresh_error(error).await;
        self.status.set_last_error(Some(error.to_string())).await;
        delay
    }

    /// Emits telemetry for successful refreshes.
    pub(crate) async fn record_refresh_success(&self, interval: Duration) {
        let telemetry = self.telemetry.read().await.clone();
        telemetry.on_refresh_success(interval);
        self.status.set_last_error(None).await;
    }

    /// Emits telemetry for failed refreshes.
    pub(crate) async fn record_refresh_error(&self, error: &ServiceError) {
        let telemetry = self.telemetry.read().await.clone();
        telemetry.on_refresh_error(error);
    }

    /// Emits telemetry when a bypass request is enqueued.
    /// Emits telemetry when a bypass request is successfully enqueued.
    pub(crate) async fn record_bypass_enqueued(&self) {
        let telemetry = self.telemetry.read().await.clone();
        telemetry.on_bypass_enqueued();
    }

    /// Emits telemetry when a bypass request times out.
    pub(crate) async fn record_bypass_timeout(&self) {
        let telemetry = self.telemetry.read().await.clone();
        telemetry.on_bypass_timeout();
    }

    /// Emits telemetry when a bypass request is rejected.
    pub(crate) async fn record_bypass_rejected(&self) {
        let telemetry = self.telemetry.read().await.clone();
        telemetry.on_bypass_rejected();
    }

    /// Emits telemetry when a bypass request is rate limited.
    pub(crate) async fn record_bypass_rate_limited(&self) {
        let telemetry = self.telemetry.read().await.clone();
        telemetry.on_bypass_rate_limited();
    }

    /// Returns the org ID encoded in the currently active RC key (if any).
    pub(crate) fn expected_org_id(&self) -> Option<u64> {
        self.rc_key
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().map(|key| key.org_id))
    }

    /// Stores the RC key snapshot used for future org-binding enforcement.
    pub(crate) fn update_rc_key(&self, key: Option<RcKey>) {
        if let Ok(mut guard) = self.rc_key.write() {
            *guard = key;
        }
    }
}

/// Extracts the refresh interval override (if any) from the backend response.
pub(crate) fn extract_refresh_override(response: &LatestConfigsResponse) -> Option<Duration> {
    let Some(meta) = &response.config_metas else {
        return None;
    };
    let Some(targets) = &meta.top_targets else {
        return None;
    };
    let json = serde_json::from_slice::<Value>(&targets.raw).ok()?;
    let seconds = json
        .pointer("/signed/custom/agent_refresh_interval")
        .and_then(Value::as_u64)?;
    if seconds == 0 {
        return None;
    }
    let duration = Duration::from_secs(seconds);
    if duration < Duration::from_secs(1) || duration > Duration::from_secs(60) {
        None
    } else {
        Some(duration)
    }
}

fn apply_immediate_retry_policy(
    streak: &mut u32,
    err: &ServiceError,
    immediate_retry: bool,
    context: &'static str,
    delay: &mut Duration,
) {
    if immediate_retry {
        if *streak < MAX_IMMEDIATE_RETRIES {
            *streak += 1;
            debug!(
                %err,
                retry_count = *streak,
                context = context,
                "remote-config scheduling immediate retry after payload failure"
            );
            *delay = Duration::from_secs(0);
        } else {
            debug!(
                %err,
                context = context,
                max_retries = MAX_IMMEDIATE_RETRIES,
                "remote-config immediate retry budget exhausted; honoring backoff"
            );
            *streak = 0;
        }
    } else {
        *streak = 0;
    }
}

/// Determines whether the provided error warrants an immediate refresh retry.
///
/// Payload-centric Uptane failures (missing blobs, stale hashes, or CDN races)
/// and short-lived metadata mismatches are typically resolved as soon as the
/// backend rehydrates target files.
/// Issuing a follow-up refresh right away lets the service recover without
/// waiting for the exponential backoff while still respecting the global
/// `MIN_REQUEST_SPACING_SECONDS` guard enforced inside `perform_refresh`.
pub(crate) fn should_retry_immediately(error: &ServiceError) -> bool {
    match error {
        ServiceError::Uptane(inner) => match inner {
            UptaneError::MissingTargetPayload { .. } => {
                // Backend omitted a referenced payload; re-fetch may supply it next time.
                true
            }
            UptaneError::UnexpectedTargetPayload { .. } => {
                // Backend sent extra payloads for stale metadata; immediate retry catches the new set.
                true
            }
            UptaneError::TargetPayloadLengthMismatch { .. } => {
                // Payload length drift often means the blob was updated mid-refresh; grab the latest copy.
                true
            }
            UptaneError::TargetPayloadHashMismatch { .. } => {
                // Hash mismatch can arise from partial propagation; retry once the backend finishes updating.
                true
            }
            UptaneError::TargetHashMismatch { .. } => {
                // Metadata hash mismatch often signals staggered director/config propagation; re-fetch once caches align.
                true
            }
            UptaneError::TargetLengthMismatch { .. } => {
                // Length mismatches mirror hash drift and typically clear up after the backend finishes rolling out targets.
                true
            }
            _ => {
                // Other Uptane errors (org mismatches, metadata divergence) require standard backoff.
                false
            }
        },
        _ => {
            // Non-Uptane failures stem from transport/auth issues; keep using exponential backoff.
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpError;
    use crate::service::test_support::{response_without_override, sample_response};

    /// Wraps an Uptane error inside a `ServiceError::Uptane` for test helpers.
    fn wrap_uptane(err: UptaneError) -> ServiceError {
        ServiceError::Uptane(err)
    }

    /// Confirms payload or metadata-related Uptane errors trigger immediate retries.
    #[test]
    fn should_retry_immediately_returns_true_for_payload_errors() {
        let cases = vec![
            wrap_uptane(UptaneError::MissingTargetPayload { path: "cfg".into() }),
            wrap_uptane(UptaneError::UnexpectedTargetPayload { path: "cfg".into() }),
            wrap_uptane(UptaneError::TargetPayloadLengthMismatch {
                path: "cfg".into(),
                expected: 10,
                actual: 8,
            }),
            wrap_uptane(UptaneError::TargetPayloadHashMismatch {
                path: "cfg".into(),
                algorithm: "sha256".into(),
            }),
            wrap_uptane(UptaneError::TargetHashMismatch {
                path: "cfg".into(),
                algorithm: "sha256".into(),
            }),
            wrap_uptane(UptaneError::TargetLengthMismatch {
                path: "cfg".into(),
                director_length: 5,
                config_length: 7,
            }),
        ];

        for error in cases {
            assert!(
                should_retry_immediately(&error),
                "expected {:?} to request immediate retry",
                error
            );
        }
    }

    /// Ensures non-payload errors stick to the normal backoff path.
    #[test]
    fn should_retry_immediately_returns_false_for_non_payload_errors() {
        let org_mismatch = wrap_uptane(UptaneError::OrgUuidMismatch {
            stored: "a".into(),
            snapshot: "b".into(),
        });
        assert!(!should_retry_immediately(&org_mismatch));

        let http_error = ServiceError::Http(HttpError::Proxy(429));
        assert!(!should_retry_immediately(&http_error));
    }

    /// Ensures responses without the custom field do not yield an override.
    #[test]
    fn override_absent_when_custom_section_missing() {
        let response = response_without_override();
        assert!(extract_refresh_override(&response).is_none());
    }

    /// Confirms a valid override integer is mapped to a bounded duration.
    #[test]
    fn override_returns_duration_when_present() {
        let response = sample_response(1, 1);
        assert_eq!(
            extract_refresh_override(&response),
            Some(Duration::from_secs(5))
        );
    }
}
