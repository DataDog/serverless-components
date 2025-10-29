//! Mutable remote-config service state and supporting helpers.
//!
//! This module encapsulates the shared state manipulated by the remote
//! configuration service.  It mirrors the Go agent bookkeeping, tracking
//! registered clients, refresh cadence, organisation status, and websocket
//! health snapshots.  The parent `service` module coordinates background
//! workers that mutate this state; consumers interact through the public API
//! exposed by `RemoteConfigService`.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use crate::bootstrap::HostRuntimeSnapshot;
use crate::http::{BackoffState, HttpError};
use crate::uptane::TufVersions;
use remote_config_proto::remoteconfig::{Client, LatestConfigsRequest, OrgStatusResponse};
use time::OffsetDateTime;
use tokio::time::Instant;

use super::{ServiceConfig, ServiceError};

/// Number of consecutive unauthorized/proxy errors to log before downgrading to debug.
pub(super) const INITIAL_FETCH_ERROR_LOG: u32 = 5;
/// Number of consecutive 503/504 errors before escalating to error logs.
pub(super) const MAX_FETCH_503_LOG_LEVEL: u32 = 5;
/// Number of consecutive org-status 503/504 errors before escalating to error logs.
pub(super) const MAX_ORG_STATUS_FETCH_503_LOG_LEVEL: u32 = 5;

/// Runtime metadata describing hostname, tags, and trace environment propagated to the backend.
#[derive(Debug, Clone)]
pub(crate) struct RuntimeMetadata {
    pub hostname: String,
    pub tags: Vec<String>,
    pub trace_agent_env: Option<String>,
}

impl RuntimeMetadata {
    /// Derives runtime metadata defaults from the static service configuration.
    pub fn from_config(config: &ServiceConfig) -> Self {
        Self {
            hostname: config.hostname.clone(),
            tags: config.tags.clone(),
            trace_agent_env: config.trace_agent_env.clone(),
        }
    }

    /// Merges a live snapshot into the metadata, falling back to defaults when unset.
    pub fn merge_snapshot(&mut self, snapshot: &HostRuntimeSnapshot, defaults: &ServiceConfig) {
        if let Some(hostname) = snapshot.hostname.as_ref() {
            self.hostname = hostname.as_ref().to_owned();
        } else if self.hostname.is_empty() {
            self.hostname = defaults.hostname.clone();
        }

        if let Some(tags) = snapshot.tags.as_ref() {
            self.tags = match tags {
                Cow::Borrowed(values) => values.to_vec(),
                Cow::Owned(values) => values.clone(),
            };
        } else if self.tags.is_empty() {
            self.tags = defaults.tags.clone();
        }

        if let Some(trace_env) = snapshot.trace_agent_env.as_ref() {
            self.trace_agent_env = Some(trace_env.as_ref().to_owned());
        } else if self.trace_agent_env.is_none() {
            self.trace_agent_env = defaults.trace_agent_env.clone();
        }
    }
}

/// Snapshot exposed to diagnostics describing the runtime metadata currently in effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeStateSnapshot {
    pub hostname: String,
    pub tags: Vec<String>,
    pub trace_agent_env: Option<String>,
}

impl From<RuntimeMetadata> for RuntimeStateSnapshot {
    /// Converts internal metadata into a consumer-friendly diagnostic structure.
    fn from(metadata: RuntimeMetadata) -> Self {
        Self {
            hostname: metadata.hostname,
            tags: metadata.tags,
            trace_agent_env: metadata.trace_agent_env,
        }
    }
}

/// Information about a registered client, exposed via diagnostics APIs.
#[derive(Debug, Clone)]
pub struct ClientSnapshot {
    /// Unique client identifier.
    pub id: String,
    /// Products that the client is subscribed to.
    pub products: Vec<String>,
    /// Duration since the client was last seen.
    pub last_seen_ago: Duration,
}

/// Outcome of registering a client request with the service state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientRegistrationOutcome {
    /// Indicates whether the client identifier is newly observed.
    pub is_new_client: bool,
    /// Marks that the client introduced products the service has not tracked yet.
    pub new_products_registered: bool,
}

impl ClientRegistrationOutcome {
    /// Returns `true` when a cache-bypass refresh should be triggered.
    pub fn should_trigger_bypass(&self) -> bool {
        self.is_new_client || self.new_products_registered
    }
}

/// Snapshot exposing high-level service state for diagnostics and testing.
#[derive(Debug, Clone)]
pub struct ServiceSnapshot {
    /// Duration until the next scheduled refresh.
    pub next_refresh: Duration,
    /// Most recent refresh error message (if any).
    pub last_error: Option<String>,
    /// Number of consecutive refresh failures.
    pub consecutive_errors: u32,
    /// Timestamp of the last successful refresh.
    pub last_success: Option<Instant>,
    /// Timestamp of the most recent refresh attempt (success or error).
    pub last_refresh: Option<Instant>,
    /// Whether the first refresh has yet to complete.
    pub first_refresh_pending: bool,
    /// Latest organisation status returned by the backend.
    pub org_status: Option<OrgStatusSnapshot>,
    /// Previously observed organisation status snapshot.
    pub previous_org_status: Option<OrgStatusSnapshot>,
    /// Result of the most recent websocket echo check.
    pub websocket_last_result: Option<WebsocketCheckResult>,
    /// Timestamp when the websocket echo check last ran.
    pub websocket_last_check: Option<Instant>,
    /// Error message captured from the most recent failed websocket check.
    pub websocket_last_error: Option<String>,
}

/// Status of the organisation flags retrieved via `/api/v0.1/status`.
#[derive(Debug, Clone, Copy)]
pub struct OrgStatusSnapshot {
    /// Whether Remote Configuration is enabled for the organisation.
    pub enabled: bool,
    /// Whether the current credentials are authorised to access Remote Config.
    pub authorized: bool,
    /// Timestamp when the status was last retrieved.
    pub fetched_at: Instant,
}

/// Result of a websocket echo check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebsocketCheckResult {
    /// The websocket handshake and echo exchange succeeded.
    Success,
    /// The websocket attempt failed (network or protocol error).
    Failure,
}

/// Log level classification for refresh failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RefreshErrorLevel {
    Debug,
    Warn,
    Error,
}

/// Internal structure encapsulating mutable service state guarded by a mutex.
#[derive(Debug)]
pub(super) struct ServiceState {
    /// Active clients registered with the service.
    pub(super) clients: HashMap<String, ClientInfo>,
    /// Products actively subscribed to remote configuration updates.
    pub(super) products: HashSet<String>,
    /// Products pending activation (reported in `new_products`).
    pub(super) new_products: HashSet<String>,
    /// Remaining budget for cache-bypass refreshes in the current window.
    pub(super) cache_bypass: CacheBypassTracker,
    /// Backoff tracker mirroring the Go exponential backoff policy.
    pub(super) backoff: BackoffState,
    /// Last refresh attempt timestamp.
    pub(super) last_attempt: Option<Instant>,
    /// Last successful refresh timestamp.
    pub(super) last_success: Option<Instant>,
    /// Cached error message from the last failed refresh.
    pub(super) last_error: Option<String>,
    /// Number of consecutive refresh failures.
    pub(super) consecutive_errors: u32,
    /// Current refresh interval applied by the poller.
    pub(super) refresh_interval: Duration,
    /// Upcoming refresh delay calculated after the previous iteration.
    pub(super) next_refresh: Duration,
    /// Timestamp when the last successful refresh completed.
    pub(super) last_update_timestamp: Option<Instant>,
    /// Whether metadata-provided refresh overrides are honoured.
    pub(super) refresh_override_allowed: bool,
    /// Indicates whether the first refresh is still pending.
    pub(super) first_refresh: bool,
    /// Timestamp of the most recent refresh attempt.
    pub(super) last_refresh: Option<Instant>,
    /// Org UUID returned by the backend (if known).
    pub(super) org_uuid: Option<String>,
    /// Latest organisation status flags.
    pub(super) org_status: Option<OrgStatusSnapshot>,
    /// Previously observed organisation status flags.
    pub(super) previous_org_status: Option<OrgStatusSnapshot>,
    /// Result of the latest websocket echo check.
    pub(super) last_websocket_result: Option<WebsocketCheckResult>,
    /// Timestamp of the latest websocket echo check.
    pub(super) last_websocket_check: Option<Instant>,
    /// Last error message produced by the websocket echo task.
    pub(super) last_websocket_error: Option<String>,
    /// Number of consecutive fetch errors seen for the current error kind.
    pub(super) fetch_error_count: u32,
    /// Number of consecutive 503/504 errors observed.
    pub(super) fetch_503_504_count: u32,
    /// Number of consecutive org-status 503/504 errors observed.
    pub(super) org_status_fetch_503_count: u32,
    /// Identifier of the last fetch error kind.
    pub(super) last_fetch_error_kind: Option<String>,
    /// Last refresh error message recorded for diagnostics.
    pub(super) last_update_error: Option<String>,
    /// Cached mapping from logical target paths to physical files plus the version it belongs to.
    pub(super) cached_target_paths: Option<(u64, Arc<HashMap<String, String>>)>,
}

impl ServiceState {
    /// Constructs the mutable state using values derived from the service configuration.
    pub(super) fn new(config: &ServiceConfig) -> Self {
        Self {
            clients: HashMap::new(),
            products: HashSet::new(),
            new_products: HashSet::new(),
            cache_bypass: CacheBypassTracker::new(
                config.cache_bypass_limit,
                config.default_refresh_interval,
            ),
            backoff: BackoffState::new(config.backoff_config),
            last_attempt: None,
            last_success: None,
            last_error: None,
            consecutive_errors: 0,
            refresh_interval: config.default_refresh_interval,
            next_refresh: config.default_refresh_interval,
            last_update_timestamp: None,
            refresh_override_allowed: config.allow_refresh_override,
            first_refresh: true,
            last_refresh: None,
            org_uuid: None,
            org_status: None,
            previous_org_status: None,
            last_websocket_result: None,
            last_websocket_check: None,
            last_websocket_error: None,
            fetch_error_count: 0,
            fetch_503_504_count: 0,
            org_status_fetch_503_count: 0,
            last_fetch_error_kind: None,
            last_update_error: None,
            cached_target_paths: None,
        }
    }

    /// Returns the delay required to honour the minimum request spacing.
    ///
    /// The poller uses this to ensure at least one second elapses between
    /// successive refresh attempts regardless of how quickly the backend
    /// responds.
    pub(super) fn time_until_next_attempt(&self, min_spacing: Duration) -> Option<Duration> {
        self.last_attempt.and_then(|attempt| {
            let now = Instant::now();
            let delta = now.saturating_duration_since(attempt);
            if delta >= min_spacing {
                None
            } else {
                Some(min_spacing - delta)
            }
        })
    }

    /// Returns the delay required to honour the timestamp resolution guard.
    ///
    /// Uptane metadata uses second-level resolution, so we wait until the next
    /// boundary before issuing another refresh to avoid duplicate timestamps.
    pub(super) fn time_until_timestamp_boundary(&self, resolution: Duration) -> Option<Duration> {
        self.last_update_timestamp.and_then(|last| {
            let target = last.checked_add(resolution)?;
            let now = Instant::now();
            if now >= target {
                None
            } else {
                Some(target - now)
            }
        })
    }

    /// Marks a refresh attempt so subsequent calls honour the minimum spacing.
    pub(super) fn record_attempt(&mut self) {
        self.last_attempt = Some(Instant::now());
    }

    /// Produces the protobuf request using the latest Uptane versions and config metadata.
    ///
    /// Mirrors the Go agent's `buildLatestConfigsRequest` by including client
    /// activity, cached error state, and backend-provided opaque state.
    pub(super) fn build_latest_configs_request(
        &mut self,
        config: &ServiceConfig,
        runtime: &RuntimeMetadata,
        versions: TufVersions,
        backend_client_state: Vec<u8>,
    ) -> LatestConfigsRequest {
        let active_clients = self.active_clients(config.clients_ttl);
        let error_message = self.last_error.clone().unwrap_or_else(String::new);
        let trace_env = runtime.trace_agent_env.clone().unwrap_or_else(String::new);
        let org_uuid = self.org_uuid.clone().unwrap_or_else(String::new);
        LatestConfigsRequest {
            hostname: runtime.hostname.clone(),
            agent_version: config.agent_version.clone(),
            current_config_snapshot_version: versions.config_snapshot,
            current_config_root_version: versions.config_root,
            current_director_root_version: versions.director_root,
            products: self.products.iter().cloned().collect(),
            new_products: self.new_products.iter().cloned().collect(),
            active_clients,
            backend_client_state,
            has_error: self.last_error.is_some(),
            error: error_message,
            trace_agent_env: trace_env,
            org_uuid,
            tags: runtime.tags.clone(),
            agent_uuid: config.agent_uuid.clone(),
        }
    }

    /// Applies the aftermath of a successful refresh and returns the next interval.
    pub(super) fn handle_refresh_success(
        &mut self,
        config: &ServiceConfig,
        maybe_override: Option<Duration>,
    ) -> Duration {
        let now = Instant::now();
        self.last_success = Some(now);
        self.last_refresh = Some(now);
        if self.first_refresh {
            self.first_refresh = false;
        }
        self.last_error = None;
        self.consecutive_errors = 0;
        self.backoff.register_success();
        self.products.extend(self.new_products.drain());
        let next = self.calculate_refresh_interval(config, maybe_override);
        self.cache_bypass.reset(self.refresh_interval);
        self.last_update_timestamp = Some(now);
        self.fetch_error_count = 0;
        self.fetch_503_504_count = 0;
        self.last_fetch_error_kind = None;
        self.last_update_error = None;
        next
    }

    /// Applies the aftermath of a failed refresh, returning the next interval and desired log level.
    pub(super) fn handle_refresh_error(
        &mut self,
        config: &ServiceConfig,
        error: &ServiceError,
    ) -> (Duration, RefreshErrorLevel) {
        self.last_refresh = Some(Instant::now());
        let message = error.to_string();
        self.last_error = Some(message.clone());
        self.last_update_error = Some(message);
        self.consecutive_errors = self.consecutive_errors.saturating_add(1);
        let backoff_delay = self.backoff.register_error();
        let computed = backoff_delay.max(config.min_refresh_interval);
        self.next_refresh = computed;

        let kind_key = classify_error_kind(error);
        if self
            .last_fetch_error_kind
            .as_ref()
            .map(|kind| kind == &kind_key)
            .unwrap_or(false)
        {
            self.fetch_error_count = self.fetch_error_count.saturating_add(1);
        } else {
            // New error family observed, reset counters so escalation happens per kind.
            self.fetch_error_count = 1;
            self.last_fetch_error_kind = Some(kind_key);
        }

        if let ServiceError::Http(HttpError::Retryable(status)) = error {
            if matches!(*status, 503 | 504) {
                self.fetch_503_504_count = self.fetch_503_504_count.saturating_add(1);
            } else {
                self.fetch_503_504_count = 0;
            }
        } else {
            self.fetch_503_504_count = 0;
        }

        (self.next_refresh, self.determine_error_level(error))
    }

    /// Records an org-status fetch error and indicates whether escalation is required.
    pub(super) fn record_org_status_fetch_error(&mut self) -> bool {
        self.org_status_fetch_503_count = self.org_status_fetch_503_count.saturating_add(1);
        self.org_status_fetch_503_count >= MAX_ORG_STATUS_FETCH_503_LOG_LEVEL
    }

    /// Resets the org-status fetch error counter.
    pub(super) fn reset_org_status_fetch_errors(&mut self) {
        self.org_status_fetch_503_count = 0;
    }

    /// Stores the most recent organisation status snapshot.
    pub(super) fn update_org_status(
        &mut self,
        status: OrgStatusResponse,
    ) -> Option<(Option<OrgStatusSnapshot>, OrgStatusSnapshot)> {
        let snapshot = OrgStatusSnapshot {
            enabled: status.enabled,
            authorized: status.authorized,
            fetched_at: Instant::now(),
        };
        let previous = self.org_status;
        let changed = match previous {
            Some(prev) => {
                prev.enabled != snapshot.enabled || prev.authorized != snapshot.authorized
            }
            None => true,
        };
        self.previous_org_status = self.org_status;
        self.org_status = Some(snapshot);
        changed.then_some((previous, snapshot))
    }

    /// Returns an immutable snapshot used by diagnostics and unit tests.
    pub(super) fn snapshot(&self) -> ServiceSnapshot {
        ServiceSnapshot {
            next_refresh: self.next_refresh,
            last_error: self.last_error.clone(),
            consecutive_errors: self.consecutive_errors,
            last_success: self.last_success,
            last_refresh: self.last_refresh,
            first_refresh_pending: self.first_refresh,
            org_status: self.org_status,
            previous_org_status: self.previous_org_status,
            websocket_last_result: self.last_websocket_result,
            websocket_last_check: self.last_websocket_check,
            websocket_last_error: self.last_websocket_error.clone(),
        }
    }

    /// Determines the log level that should be used for the given refresh error.
    fn determine_error_level(&self, error: &ServiceError) -> RefreshErrorLevel {
        let org_enabled = self
            .previous_org_status
            .or(self.org_status)
            .map(|status| status.enabled && status.authorized)
            .unwrap_or(false);

        match error {
            ServiceError::Http(HttpError::Unauthorized)
            | ServiceError::Http(HttpError::Proxy(_)) => {
                if self.fetch_error_count > INITIAL_FETCH_ERROR_LOG {
                    RefreshErrorLevel::Debug
                } else if org_enabled {
                    RefreshErrorLevel::Warn
                } else {
                    RefreshErrorLevel::Debug
                }
            }
            ServiceError::Http(HttpError::Retryable(status)) if matches!(*status, 503 | 504) => {
                if !org_enabled {
                    RefreshErrorLevel::Debug
                } else if self.fetch_503_504_count >= MAX_FETCH_503_LOG_LEVEL {
                    RefreshErrorLevel::Error
                } else {
                    RefreshErrorLevel::Warn
                }
            }
            _ => {
                if org_enabled {
                    RefreshErrorLevel::Warn
                } else {
                    RefreshErrorLevel::Debug
                }
            }
        }
    }

    /// Removes client entries whose last activity exceeds the configured TTL.
    ///
    /// This keeps diagnostics and `active_clients` lists bounded even when
    /// clients disappear without explicitly deregistering.
    fn prune_stale_clients(&mut self, ttl: Duration) {
        let now = Instant::now();
        self.clients
            .retain(|_, info| now.saturating_duration_since(info.last_seen) <= ttl);
    }

    /// Registers or refreshes an active client and returns the registration outcome.
    ///
    /// The method mirrors the Go agent: stale entries are pruned, new products
    /// are tracked separately (so the next refresh can report them as
    /// `new_products`), and `last_seen` timestamps are updated atomically.
    pub(super) fn register_client(
        &mut self,
        descriptor: &Client,
        ttl: Duration,
    ) -> ClientRegistrationOutcome {
        self.prune_stale_clients(ttl);
        let now = Instant::now();
        let products: HashSet<String> = descriptor.products.iter().cloned().collect();
        let mut unknown_products = Vec::new();
        for product in &products {
            if !self.products.contains(product) {
                unknown_products.push(product.clone());
            }
        }
        let client_id = descriptor.id.clone();
        let mut sanitized = descriptor.clone();
        sanitized.last_seen = current_unix_millis();
        let mut is_new_client = false;
        if let Some(info) = self.clients.get_mut(&client_id) {
            for product in &products {
                if !info.products.contains(product) {
                    self.new_products.insert(product.clone());
                }
            }
            info.last_seen = now;
            info.products.extend(products.iter().cloned());
            info.descriptor = sanitized;
        } else {
            self.clients.insert(
                client_id,
                ClientInfo {
                    last_seen: now,
                    products,
                    descriptor: sanitized,
                },
            );
            is_new_client = true;
        }
        let has_new_products = !unknown_products.is_empty();
        self.new_products.extend(unknown_products);
        ClientRegistrationOutcome {
            is_new_client,
            new_products_registered: has_new_products,
        }
    }

    /// Returns active client descriptors with updated `last_seen` timestamps.
    pub(super) fn active_clients(&mut self, ttl: Duration) -> Vec<Client> {
        self.prune_stale_clients(ttl);
        let last_seen = current_unix_millis();
        self.clients
            .values_mut()
            .map(|info| {
                info.descriptor.last_seen = last_seen;
                info.descriptor.clone()
            })
            .collect()
    }

    /// Calculates the next refresh interval, respecting backend overrides when allowed.
    fn calculate_refresh_interval(
        &mut self,
        config: &ServiceConfig,
        maybe_override: Option<Duration>,
    ) -> Duration {
        let mut interval = config.default_refresh_interval;
        if self.refresh_override_allowed {
            if let Some(override_interval) = maybe_override {
                if override_interval >= config.min_refresh_interval
                    && override_interval <= Duration::from_secs(60)
                {
                    // Backend-provided interval overrides are clamped to
                    // sensible values just like in the Go agent.
                    interval = override_interval;
                }
            }
        }
        self.refresh_interval = interval;
        self.next_refresh = interval.max(config.min_refresh_interval);
        self.next_refresh
    }

    /// Returns (and caches) the logical→physical path mapping for the provided director version.
    pub(super) fn director_path_mapping(
        &mut self,
        director_version: u64,
        targets_raw: &[u8],
    ) -> Result<Arc<HashMap<String, String>>, ServiceError> {
        if let Some((cached_version, mapping)) = &self.cached_target_paths {
            if *cached_version == director_version {
                return Ok(mapping.clone());
            }
        }
        let targets_json = serde_json::from_slice(targets_raw).map_err(ServiceError::Json)?;
        let mapping = Arc::new(super::client::build_path_mapping(targets_json)?);
        self.cached_target_paths = Some((director_version, mapping.clone()));
        Ok(mapping)
    }

    /// Clears the cached path mapping (used when metadata is refreshed).
    pub(super) fn clear_cached_paths(&mut self) {
        self.cached_target_paths = None;
    }

    /// Returns a bypass slot to the tracker when an enqueue attempt fails.
    pub(super) fn refund_bypass_slot(&mut self) {
        self.cache_bypass.refund();
    }

    /// Returns the remaining bypass allowance (test diagnostics only).
    #[cfg(test)]
    pub(super) fn cache_bypass_remaining(&self) -> usize {
        self.cache_bypass.remaining
    }
}

/// Representation of an active client that is polling for remote configurations.
#[derive(Debug)]
pub(super) struct ClientInfo {
    /// Instant when the client was last seen.
    pub(super) last_seen: Instant,
    /// Products requested by the client.
    pub(super) products: HashSet<String>,
    /// Most recent descriptor reported by the client.
    pub(super) descriptor: Client,
}

/// Tracks cache-bypass allowance within a refresh window.
///
/// This mirrors the Go agent's fixed-window rate limiter: new clients can
/// trigger at most `capacity` bypass refreshes per refresh interval so the
/// backend is not overwhelmed when many tracers start simultaneously.
#[derive(Debug, Clone)]
pub(super) struct CacheBypassTracker {
    /// Number of bypass requests permitted per window.
    capacity: usize,
    /// Remaining bypass requests in the current window.
    remaining: usize,
    /// Start instant of the current window.
    window_start: Instant,
    /// Duration of the active refresh window.
    window: Duration,
}

impl CacheBypassTracker {
    /// Creates a new tracker with the provided capacity and window duration.
    pub(super) fn new(capacity: usize, window: Duration) -> Self {
        Self {
            capacity,
            remaining: capacity,
            window_start: Instant::now(),
            window,
        }
    }

    /// Resets the tracker when a refresh successfully completes.
    pub(super) fn reset(&mut self, window: Duration) {
        self.remaining = self.capacity;
        self.window_start = Instant::now();
        self.window = window;
    }

    /// Attempts to consume one bypass slot, honouring the configured window.
    pub(super) fn try_consume(&mut self) -> bool {
        if Instant::now().duration_since(self.window_start) >= self.window {
            // Reset the window when the elapsed time exceeds the configured duration.
            self.reset(self.window);
        }
        if self.remaining > 0 {
            self.remaining -= 1;
            true
        } else {
            false
        }
    }

    /// Returns a bypass slot to the pool (used when enqueue attempts fail).
    pub(super) fn refund(&mut self) {
        if self.remaining < self.capacity {
            self.remaining += 1;
        }
    }

    /// Returns the current refresh window duration enforced by the tracker.
    #[cfg(test)]
    pub(super) fn window(&self) -> Duration {
        self.window
    }
}

/// Returns a stable identifier for the provided error kind.
///
/// Used to group consecutive failures by error family so we can decide when to
/// downgrade logging or escalate to errors.
pub(super) fn classify_error_kind(error: &ServiceError) -> String {
    match error {
        ServiceError::Http(HttpError::Unauthorized) => "unauthorized".into(),
        ServiceError::Http(HttpError::Proxy(code)) => format!("proxy-{code}"),
        ServiceError::Http(HttpError::Retryable(code)) => format!("retryable-{code}"),
        ServiceError::Http(HttpError::InsecureUrl(_)) => "insecure-url".into(),
        ServiceError::Http(HttpError::Transport(_)) => "transport".into(),
        ServiceError::Http(HttpError::Decode(_)) => "decode".into(),
        ServiceError::Http(HttpError::FipsConfig(_)) => "fips-config".into(),
        ServiceError::Uptane(_) => "uptane".into(),
        ServiceError::InvalidRequest(_) => "invalid-request".into(),
        ServiceError::Json(_) => "json".into(),
        ServiceError::Protobuf(_) => "protobuf".into(),
        ServiceError::EmbeddedRoot(_) => "embedded-root".into(),
    }
}

/// Returns the current Unix timestamp in milliseconds.
///
/// The value is stored in `Client.last_seen` offsets so diagnostics match the
/// Go agent’s output.
fn current_unix_millis() -> u64 {
    let now = OffsetDateTime::now_utc();
    now.unix_timestamp_nanos()
        .saturating_div(1_000_000)
        .try_into()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_targets_blob() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "signed": {
                "targets": {
                    "datadog/2/APM_TRACING/config/foo.json": {
                        "length": 10,
                        "hashes": { "sha256": "deadbeef" }
                    }
                }
            }
        }))
        .expect("serialize targets")
    }

    #[test]
    fn director_path_mapping_reuses_cache_for_same_version() {
        let mut state = ServiceState::new(&ServiceConfig::default());
        let blob = sample_targets_blob();
        let first = state
            .director_path_mapping(5, &blob)
            .expect("initial mapping");
        assert_eq!(
            first.get("datadog/2/APM_TRACING/config/foo.json"),
            Some(&"datadog/2/APM_TRACING/config/deadbeef.foo.json".to_string())
        );

        // Pass invalid JSON with the same version; cache should still serve the last mapping.
        let reuse = state
            .director_path_mapping(5, b"not-json")
            .expect("cached mapping reused");
        assert_eq!(first, reuse);
    }

    #[test]
    fn director_path_mapping_reparses_after_cache_clear() {
        let mut state = ServiceState::new(&ServiceConfig::default());
        let blob = sample_targets_blob();
        state
            .director_path_mapping(2, &blob)
            .expect("initial mapping");
        state.clear_cached_paths();

        let err = state
            .director_path_mapping(2, b"not-json")
            .expect_err("cache cleared should force reparse");
        assert!(matches!(err, ServiceError::Json(_)));
    }
}
