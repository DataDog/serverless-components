//! Remote configuration service orchestration.
//!
//! This module wires the HTTP client, Uptane storage, and cache housekeeping
//! logic into an asynchronous service that mirrors the behaviour of the Go
//! agent.  It is responsible for refreshing remote configuration payloads,
//! polling organisation status, and honouring cache-bypass requests issued
//! by freshly registered clients.

use std::sync::{Arc, Weak};

use super::client::{build_cached_index, compute_matched_configs, validate_client_request};
use super::config::ServiceConfig;
use super::refresh::ServiceShared;
use super::state::{
    ClientSnapshot, RuntimeMetadata, RuntimeStateSnapshot, ServiceSnapshot, ServiceState,
};
use super::telemetry::{NoopTelemetry, RemoteConfigTelemetry};
use super::util::compute_sha256;
use crate::bootstrap::HostRuntimeSnapshot;
use crate::http::{HttpClient, HttpError};
use crate::store::StoreError;
use crate::uptane::{UptaneError, UptaneState};
use prost::Message;
use remote_config_proto::remoteconfig::{
    Client, ClientGetConfigsRequest, ClientGetConfigsResponse, ConfigStatus, File, TargetFileMeta,
};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration, Instant};
use tracing::{debug, warn};

/// Number of seconds the service enforces between consecutive HTTP refreshes.
/// Channel capacity used for cache-bypass triggers (1 mirrors the Go buffered channel).
const CACHE_BYPASS_CHANNEL_CAPACITY: usize = 1;

/// Error type surfaced by the remote configuration service operations.
#[derive(Debug, Error)]
pub enum ServiceError {
    /// Propagates HTTP-layer failures (network issues or HTTP errors).
    #[error("http error: {0}")]
    Http(#[from] HttpError),
    /// Surfaces Uptane state machine failures.
    #[error("uptane error: {0}")]
    Uptane(#[from] UptaneError),
    /// Indicates that the client request payload was invalid.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// JSON serialisation or canonicalisation failure.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// Protobuf decode failure.
    #[error("protobuf decode error: {0}")]
    Protobuf(#[from] prost::DecodeError),
    /// Embedded trust root resolver failure.
    #[error("embedded root error: {0}")]
    EmbeddedRoot(String),
}

/// Handle returned by [`RemoteConfigService::start`] to manage background tasks.
pub struct RemoteConfigHandle {
    /// Sender used to broadcast shutdown notifications to spawned tasks.
    shutdown: broadcast::Sender<()>,
    /// Join handles for the spawned background tasks.
    join_handles: Vec<JoinHandle<()>>,
    /// Reference to the service for cleanup operations during shutdown.
    service: RemoteConfigService,
}

impl std::fmt::Debug for RemoteConfigHandle {
    /// Prints the number of background tasks without leaking handles.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteConfigHandle")
            .field("shutdown", &self.shutdown)
            .field("join_handles", &self.join_handles.len())
            .finish()
    }
}

impl RemoteConfigHandle {
    /// Constructs a handle from pre-existing shutdown and join handles.
    ///
    /// This helper is only used in tests where we spawn dummy tasks and need to
    /// wrap them in a `RemoteConfigHandle` without going through `start()`.
    #[cfg(test)]
    pub(crate) fn from_parts(
        shutdown: broadcast::Sender<()>,
        join_handles: Vec<JoinHandle<()>>,
        service: RemoteConfigService,
    ) -> Self {
        Self {
            shutdown,
            join_handles,
            service,
        }
    }

    /// Stops background workers and waits for all tasks to terminate.
    pub async fn shutdown(self) {
        let RemoteConfigHandle {
            shutdown,
            mut join_handles,
            service,
        } = self;
        // Send shutdown signal to all subscribers.
        let _ = shutdown.send(());

        // Abort all background tasks immediately to ensure fast shutdown.
        // Since we're shutting down the entire agent, we don't need graceful
        // exit - we can just kill the tasks and flush the database.
        for handle in join_handles.drain(..) {
            handle.abort();
            // Wait for the task to fully clean up and drop its Arc references
            let _ = handle.await;
        }

        // Abort runtime/credential watcher tasks registered outside `start()`.
        service.abort_aux_tasks().await;

        // Flush the database to ensure all pending writes are committed
        if let Err(err) = service.uptane_state().flush() {
            warn!("failed to flush remote-config database during shutdown: {err}");
        }

        // Drop the service explicitly to close the Sled database and stop its
        // background flusher thread
        drop(service);
    }
}

/// Signal sent through the cache-bypass channel to force an immediate refresh.
#[derive(Debug)]
pub struct CacheBypassSignal {
    /// Optional channel used to notify the requester once the refresh completes.
    pub completion: Option<oneshot::Sender<Result<(), String>>>,
}

/// Reason why a cache-bypass refresh was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BypassTrigger {
    /// Newly observed client identifier requested configs.
    NewClient,
    /// Existing client reported new products that require backend awareness.
    NewProducts,
}

impl BypassTrigger {
    /// Returns a descriptive label for tracing/logging purposes.
    fn as_str(&self) -> &'static str {
        match self {
            Self::NewClient => "new-client",
            Self::NewProducts => "new-products",
        }
    }
}

/// Outcome of a refresh attempt including the next refresh interval.
#[derive(Debug, Clone, Copy)]
pub struct RefreshOutcome {
    /// Delay recommended before the next refresh attempt.
    pub next_interval: Duration,
}

/// Snapshot describing the current Uptane state and client activity.
#[derive(Debug, Clone)]
pub struct ConfigStateSnapshot {
    /// Stored Uptane metadata and target hashes.
    pub uptane: crate::uptane::State,
    /// Active clients known to the service.
    pub active_clients: Vec<Client>,
}

/// Remote configuration service responsible for backend interactions.
#[derive(Debug, Clone)]
pub struct RemoteConfigService {
    /// Shared service internals wrapped in an atomically reference-counted pointer.
    shared: Arc<ServiceShared>,
}

impl RemoteConfigService {
    /// Builds a new service instance using the provided HTTP client, Uptane state, and configuration.
    ///
    /// The configuration is sanitised up-front so callers inherit the same
    /// safety limits as the Go agent even if they forget to call
    /// `ServiceConfig::sanitise()` themselves.
    pub fn new(http_client: HttpClient, uptane: UptaneState, config: ServiceConfig) -> Self {
        let config = config.sanitise();
        let state = ServiceState::new(&config);
        let runtime_metadata = RuntimeMetadata::from_config(&config);
        let rc_key_snapshot = config.rc_key.clone();
        let shared = ServiceShared {
            http_client,
            uptane,
            state: Mutex::new(state),
            cache_bypass_tx: Mutex::new(None),
            telemetry: RwLock::new(Arc::new(NoopTelemetry)),
            status: crate::status::RemoteConfigStatus::new(),
            runtime: RwLock::new(runtime_metadata),
            config,
            rc_key: std::sync::RwLock::new(rc_key_snapshot),
            aux_tasks: Mutex::new(Vec::new()),
        };
        Self {
            shared: Arc::new(shared),
        }
    }

    /// Reconstructs a service wrapper from shared state (used when upgrading weak pointers).
    pub(crate) fn from_shared(shared: Arc<ServiceShared>) -> Self {
        Self { shared }
    }

    /// Produces a weak reference to the shared state so background tasks can detect shutdown.
    pub(crate) fn downgrade(&self) -> Weak<ServiceShared> {
        Arc::downgrade(&self.shared)
    }

    /// Spawns background workers (refresh loop, organisation poller) and returns a handle controlling them.
    pub async fn start(&self) -> RemoteConfigHandle {
        let (shutdown_tx, _) = broadcast::channel(1);
        let (bypass_tx, bypass_rx) = mpsc::channel(CACHE_BYPASS_CHANNEL_CAPACITY);

        {
            let mut guard = self.shared.cache_bypass_tx.lock().await;
            *guard = Some(bypass_tx.clone());
        }

        let mut join_handles = Vec::new();
        if self.shared.config.disable_background_poller {
            // Manual mode: only process cache-bypass requests so new clients can force refreshes.
            let shared = self.shared.clone();
            let mut shutdown_rx = shutdown_tx.subscribe();
            let handle = tokio::spawn(async move {
                shared
                    .run_on_demand_poller(bypass_rx, &mut shutdown_rx)
                    .await;
            });
            join_handles.push(handle);
        } else {
            // Default mode: run the periodic poller that blends scheduled refreshes with bypass signals.
            let shared = self.shared.clone();
            let mut shutdown_rx = shutdown_tx.subscribe();
            let handle = tokio::spawn(async move {
                shared.run_config_poller(bypass_rx, &mut shutdown_rx).await;
            });
            join_handles.push(handle);
        }

        // Organisation status poller runs irrespective of the config poller setting
        // so we keep reporting enablement/authorization transitions.
        let shared = self.shared.clone();
        let mut shutdown_rx = shutdown_tx.subscribe();
        let handle = tokio::spawn(async move {
            shared.run_org_status_poller(&mut shutdown_rx).await;
        });
        join_handles.push(handle);

        if self.shared.config.enable_websocket_echo {
            let shared = self.shared.clone();
            let mut shutdown_rx = shutdown_tx.subscribe();
            let handle = tokio::spawn(async move {
                shared.run_websocket_echo(&mut shutdown_rx).await;
            });
            join_handles.push(handle);
        }

        RemoteConfigHandle {
            shutdown: shutdown_tx,
            join_handles,
            service: self.clone(),
        }
    }

    /// Performs a single refresh cycle synchronously (useful for tests and manual bootstrapping).
    pub async fn refresh_once(&self) -> Result<(), ServiceError> {
        match self.shared.perform_refresh().await {
            Ok(_) => Ok(()),
            Err(err) => {
                self.shared.handle_refresh_error(&err).await;
                Err(err)
            }
        }
    }

    /// Returns a diagnostics snapshot describing the current service state.
    pub async fn snapshot(&self) -> ServiceSnapshot {
        let guard = self.shared.state.lock().await;
        guard.snapshot()
    }

    /// Applies a runtime snapshot (hostname/tags/trace env) provided by the host agent.
    pub async fn apply_runtime_snapshot(&self, snapshot: HostRuntimeSnapshot) {
        self.shared.apply_runtime_snapshot(snapshot).await;
    }

    /// Returns the current runtime metadata used for refresh requests.
    pub async fn runtime_state(&self) -> RuntimeStateSnapshot {
        let metadata = self.shared.runtime_metadata().await;
        metadata.into()
    }

    /// Recreates the Uptane store and resets the in-memory service state.
    pub async fn reset_state(&self) -> Result<(), ServiceError> {
        self.shared.reset_state().await
    }

    /// Returns a handle exposing high-level status flags (org enabled/authorized, last error).
    pub fn status_handle(&self) -> Arc<crate::status::RemoteConfigStatus> {
        self.shared.status.clone()
    }

    /// Returns a composite snapshot of the Uptane store and active clients.
    pub async fn config_state(&self) -> Result<ConfigStateSnapshot, ServiceError> {
        let uptane = self.shared.uptane.state().map_err(ServiceError::Uptane)?;
        let active_clients = {
            let mut guard = self.shared.state.lock().await;
            guard.active_clients(self.shared.config.clients_ttl)
        };
        Ok(ConfigStateSnapshot {
            uptane,
            active_clients,
        })
    }

    /// Retrieves a clone of the underlying Uptane state manager for inspection.
    pub fn uptane_state(&self) -> UptaneState {
        self.shared.uptane.clone()
    }

    /// Returns a reference to the static service configuration.
    pub fn config(&self) -> &ServiceConfig {
        &self.shared.config
    }

    /// Returns the shared internals for test support code (test builds only).
    #[cfg(test)]
    pub(crate) fn shared_for_tests(&self) -> &Arc<ServiceShared> {
        &self.shared
    }

    /// Returns a cache-bypass sender that can be used to request immediate refreshes.
    pub async fn cache_bypass_sender(&self) -> Option<mpsc::Sender<CacheBypassSignal>> {
        let guard = self.shared.cache_bypass_tx.lock().await;
        guard.clone()
    }

    /// Returns a cache-bypass slot to the tracker when an enqueue attempt fails.
    async fn refund_bypass_slot(&self) {
        let mut guard = self.shared.state.lock().await;
        guard.refund_bypass_slot();
    }

    /// Refunds a reserved cache-bypass slot (no-op when nothing was reserved).
    async fn refund_reserved_bypass_slot(&self, reserved: &mut bool) {
        if *reserved {
            self.refund_bypass_slot().await;
            *reserved = false;
        }
    }

    /// Replaces the telemetry sink used for reporting service metrics.
    ///
    /// Callers typically install `CountingTelemetry` or `CompositeTelemetry`
    /// right after bootstrap so background tasks can emit metrics.
    pub async fn set_telemetry(&self, telemetry: Arc<dyn RemoteConfigTelemetry>) {
        let mut guard = self.shared.telemetry.write().await;
        *guard = telemetry;
    }

    /// Records an auxiliary background task so shutdown can abort it later.
    pub async fn register_aux_task(&self, handle: JoinHandle<()>) {
        self.shared.register_aux_task(handle).await;
    }

    /// Aborts every registered auxiliary task, ensuring they drop their references.
    pub(crate) async fn abort_aux_tasks(&self) {
        self.shared.abort_aux_tasks().await;
    }

    /// Rotates the authentication material used by the service.
    ///
    /// This updates HTTP headers in-flight and rewrites the on-disk metadata so the
    /// cached store remains associated with the latest credentials.
    pub async fn rotate_credentials(
        &self,
        api_key: &str,
        rc_key: Option<&str>,
        par_jwt: Option<&str>,
    ) -> Result<(), ServiceError> {
        self.shared
            .rotate_credentials(api_key, rc_key, par_jwt)
            .await
    }

    /// Clears cached metadata/targets and reinitialises the in-memory state.
    pub async fn reset_cache(&self) -> Result<(), ServiceError> {
        self.shared.reset_state().await
    }

    /// Handles a client `ClientGetConfigs` request, returning the metadata and targets to serve.
    pub async fn client_get_configs(
        &self,
        request: ClientGetConfigsRequest,
    ) -> Result<ClientGetConfigsResponse, ServiceError> {
        let ClientGetConfigsRequest {
            client,
            cached_target_files,
        } = request;
        let client = client.ok_or_else(|| {
            ServiceError::InvalidRequest(
                "client is a required field for client config update requests".to_string(),
            )
        })?;
        validate_client_request(&client, &cached_target_files)?;
        let mut guard = self.shared.state.lock().await;
        let registration = guard.register_client(&client, self.shared.config.clients_ttl);
        let bypass_trigger = if registration.should_trigger_bypass() {
            if registration.is_new_client {
                Some(BypassTrigger::NewClient)
            } else {
                Some(BypassTrigger::NewProducts)
            }
        } else {
            None
        };
        let mut bypass_reserved = false;
        let bypass_allowed = if bypass_trigger.is_some() {
            // Cache-bypass requests (new clients or newly advertised products) share the same budget.
            let allowed = guard.cache_bypass.try_consume();
            if allowed {
                bypass_reserved = true;
            }
            allowed
        } else {
            false
        };
        let bypass_rate_limited = bypass_trigger.is_some() && !bypass_allowed;
        drop(guard);

        if bypass_rate_limited {
            self.shared.record_bypass_rate_limited().await;
        }

        if let Some(trigger) = bypass_trigger {
            debug!(
                "remote-config: cache bypass requested for {}",
                trigger.as_str()
            );
            // When the bypass was triggered we try to refresh immediately so the client receives configs promptly.
            if let Some(sender) = self.cache_bypass_sender().await {
                if bypass_allowed {
                    let (tx, rx) = oneshot::channel();
                    let enqueue_timeout = self.shared.config.new_client_block_timeout;
                    let enqueue_start = Instant::now();
                    let send_future = sender.send(CacheBypassSignal {
                        completion: Some(tx),
                    });
                    // Wait for the bypass request to be enqueued; failure to
                    // send or timeout will surface telemetry so operators can investigate.
                    match timeout(enqueue_timeout, send_future).await {
                        Ok(Ok(())) => {
                            self.shared.record_bypass_enqueued().await;
                            let elapsed = enqueue_start.elapsed();
                            let remaining =
                                enqueue_timeout.checked_sub(elapsed).unwrap_or_default();
                            let wait_timeout = if remaining.is_zero() {
                                Duration::from_millis(0)
                            } else {
                                remaining
                            }
                            .min(self.shared.config.bypass_block_timeout);
                            if wait_timeout.is_zero() {
                                warn!(
                                    "remote-config bypass completion window elapsed immediately; skipping wait"
                                );
                                self.shared.record_bypass_timeout().await;
                            } else if let Ok(result) = timeout(wait_timeout, rx).await {
                                if let Err(message) = result
                                    .unwrap_or_else(|_| Err("refresh channel closed".to_string()))
                                {
                                    warn!(
                                        "remote-config bypass refresh reported failure: {message}"
                                    );
                                    self.shared.record_bypass_rejected().await;
                                }
                            } else {
                                warn!(
                                    "remote-config bypass refresh timed out after {:?}",
                                    wait_timeout
                                );
                                self.shared.record_bypass_timeout().await;
                            }
                        }
                        Ok(Err(_)) => {
                            warn!("remote-config failed to queue bypass refresh");
                            self.shared.record_bypass_rejected().await;
                            self.refund_reserved_bypass_slot(&mut bypass_reserved).await;
                        }
                        Err(_) => {
                            warn!(
                                "remote-config bypass enqueue blocked for {:?}; skipping immediate refresh",
                                enqueue_timeout
                            );
                            self.shared.record_bypass_timeout().await;
                            self.refund_reserved_bypass_slot(&mut bypass_reserved).await;
                        }
                    }
                } else {
                    debug!("remote-config bypass limit exhausted; relying on scheduled poll");
                    self.shared.record_bypass_rejected().await;
                }
            } else if let Err(err) = self.refresh_once().await {
                // When bypass signalling isn't available (unlikely in prod but
                // common in tests), attempt a synchronous refresh so the waiting
                // client can still get fresh configs.
                warn!("remote-config on-demand refresh failed: {err}");
                self.shared.record_bypass_rejected().await;
                self.refund_reserved_bypass_slot(&mut bypass_reserved).await;
            }
        }

        self.build_client_response(&client, &cached_target_files)
            .await
    }

    /// Handles a client request provided as protobuf-encoded bytes.
    pub async fn client_get_configs_from_bytes(
        &self,
        payload: &[u8],
    ) -> Result<Vec<u8>, ServiceError> {
        let request = ClientGetConfigsRequest::decode(payload)?;
        let response = self.client_get_configs(request).await?;
        Ok(response.encode_to_vec())
    }

    /// Forces a refresh immediately, bypassing spacing constraints.
    pub async fn force_refresh(&self) -> Result<RefreshOutcome, ServiceError> {
        {
            let mut guard = self.shared.state.lock().await;
            guard.last_attempt = None;
        }
        self.shared.perform_refresh().await
    }

    /// Records additional products that should be requested on the next refresh.
    pub async fn refresh_products<I>(&self, products: I)
    where
        I: IntoIterator<Item = String>,
    {
        let mut guard = self.shared.state.lock().await;
        for product in products {
            if !guard.products.contains(&product) {
                guard.new_products.insert(product);
            }
        }
    }

    /// Returns the current refresh interval used by the background poller.
    pub async fn get_refresh_interval(&self) -> Duration {
        let guard = self.shared.state.lock().await;
        guard.refresh_interval
    }

    /// Returns snapshots for registered clients including the products they track.
    pub async fn client_state(&self) -> Vec<ClientSnapshot> {
        let guard = self.shared.state.lock().await;
        let now = Instant::now();
        guard
            .clients
            .iter()
            .map(|(id, info)| ClientSnapshot {
                id: id.clone(),
                products: info.products.iter().cloned().collect(),
                last_seen_ago: now.saturating_duration_since(info.last_seen),
            })
            .collect()
    }

    /// Generates a response instructing clients to flush their local cache.
    pub fn flush_cache_response() -> ClientGetConfigsResponse {
        Self::flush_cache_response_with_targets(Vec::new())
    }

    /// Builds a flush response embedding the current targets metadata.
    fn flush_cache_response_with_targets(targets: Vec<u8>) -> ClientGetConfigsResponse {
        ClientGetConfigsResponse {
            roots: Vec::new(),
            targets,
            target_files: Vec::new(),
            client_configs: Vec::new(),
            config_status: ConfigStatus::Expired as i32,
        }
    }

    /// Builds a response for the client based on the current Uptane state.
    async fn build_client_response(
        &self,
        client: &Client,
        cached_target_files: &[TargetFileMeta],
    ) -> Result<ClientGetConfigsResponse, ServiceError> {
        debug!("remote-config: Building client response");

        let client_state = client
            .state
            .as_ref()
            .expect("client state must be present after validation");

        debug!(
            "remote-config: Client state - root_version: {}, targets_version: {}",
            client_state.root_version, client_state.targets_version
        );

        let versions = match self.shared.uptane.tuf_versions() {
            Ok(versions) => {
                debug!(
                    "remote-config: Uptane versions - director_root: {}, director_targets: {}, config_root: {}, config_snapshot: {}",
                    versions.director_root, versions.director_targets, versions.config_root, versions.config_snapshot
                );
                versions
            }
            Err(UptaneError::Store(StoreError::MissingMetadata)) => {
                debug!(
                    "remote-config: No Uptane metadata available yet (cold start); returning empty response"
                );
                return Ok(ClientGetConfigsResponse::default());
            }
            Err(err) => return Err(ServiceError::Uptane(err)),
        };

        let first_refresh_pending = {
            let guard = self.shared.state.lock().await;
            guard.first_refresh
        };

        debug!(
            "remote-config: First refresh pending: {}",
            first_refresh_pending
        );

        if !first_refresh_pending {
            if let Some(expires_at) = self.shared.uptane.timestamp_expires()? {
                if expires_at <= OffsetDateTime::now_utc() {
                    warn!(
                        "remote-config timestamp expired at {}; flushing client caches",
                        expires_at
                    );
                    let targets = self.shared.uptane.director_targets_raw()?;
                    return Ok(Self::flush_cache_response_with_targets(targets));
                }
            }
        }

        if versions.director_targets == client_state.targets_version {
            debug!("remote-config: Client already has latest targets version, returning empty response");
            return Ok(ClientGetConfigsResponse::default());
        }

        self.shared.verify_snapshot_org_uuid().await?;

        let matched_configs = self.matched_client_configs(client)?;
        debug!(
            "remote-config: Matched {} configs for client",
            matched_configs.len()
        );
        if !matched_configs.is_empty() {
            for (idx, config) in matched_configs.iter().enumerate() {
                debug!("remote-config:   matched_config[{}]: {}", idx, config);
            }
        }

        let roots = self.collect_roots(client_state.root_version, versions.director_root)?;
        debug!("remote-config: Collected {} root(s)", roots.len());

        let targets_raw = self.shared.uptane.director_targets_raw()?;
        debug!(
            "remote-config: Targets metadata size: {} bytes",
            targets_raw.len()
        );

        let target_files = self
            .collect_target_files(
                &matched_configs,
                cached_target_files,
                versions.director_targets,
            )
            .await?;
        debug!(
            "remote-config: Returning {} target_files to client",
            target_files.len()
        );

        Ok(ClientGetConfigsResponse {
            roots,
            targets: targets_raw,
            target_files,
            client_configs: matched_configs,
            config_status: ConfigStatus::Ok as i32,
        })
    }

    /// Resolves the configuration paths that match the provided client predicates.
    fn matched_client_configs(&self, client: &Client) -> Result<Vec<String>, ServiceError> {
        // Load the director targets metadata and mirror the Go predicate evaluation rules.
        let targets = match self.shared.uptane.director_targets() {
            Ok(value) => value,
            Err(UptaneError::Store(StoreError::MissingMetadata)) => return Ok(Vec::new()),
            Err(err) => return Err(ServiceError::Uptane(err)),
        };
        compute_matched_configs(client, targets, self.shared.expected_org_id())
    }

    /// Collects the latest root metadata blobs that should be returned to clients.
    fn collect_roots(
        &self,
        client_root_version: u64,
        latest_root_version: u64,
    ) -> Result<Vec<Vec<u8>>, ServiceError> {
        let roots = self
            .shared
            .uptane
            .director_roots_since(client_root_version, latest_root_version)
            .map_err(ServiceError::Uptane)?;
        Ok(roots)
    }

    /// Gathers target files corresponding to matched configs that are not already cached by the client.
    async fn collect_target_files(
        &self,
        matched_paths: &[String],
        cached_target_files: &[TargetFileMeta],
        director_targets_version: u64,
    ) -> Result<Vec<File>, ServiceError> {
        debug!("remote-config: Collecting target files from local store");
        debug!("remote-config:   matched_paths: {}", matched_paths.len());
        debug!(
            "remote-config:   cached_target_files (from client): {}",
            cached_target_files.len()
        );

        // Build mapping from logical paths to physical paths using TUF targets metadata
        let targets_raw = self.shared.uptane.director_targets_raw()?;
        let path_mapping = {
            let mut guard = self.shared.state.lock().await;
            guard.director_path_mapping(director_targets_version, &targets_raw)?
        };

        debug!(
            "remote-config:   Built path mapping with {} entries",
            path_mapping.len()
        );

        let cached = build_cached_index(cached_target_files);
        let mut response = Vec::new();
        for logical_path in matched_paths {
            let logical_path = logical_path.as_str();
            debug!("remote-config:   Preparing target '{}'", logical_path);
            let physical_path = path_mapping.get(logical_path).map(String::as_str);
            let raw = self.fetch_target_payload(logical_path, physical_path)?;
            let hash = compute_sha256(&raw);
            let length = raw.len() as i64;
            let already_cached = cached
                .get(logical_path)
                .map(|meta| meta.matches(length, &hash))
                .unwrap_or(false);
            if already_cached {
                debug!(
                    "remote-config:   Skipping '{}' (already cached by client)",
                    logical_path
                );
                continue;
            }
            debug!(
                "remote-config:   Including '{}' in response ({} bytes)",
                logical_path,
                raw.len()
            );
            response.push(File {
                path: logical_path.to_string(), // Return logical path to client
                raw,
            });
        }

        debug!(
            "remote-config: Collected {} target_files for response",
            response.len()
        );
        Ok(response)
    }

    /// Loads a target payload from the cache, falling back to legacy physical paths when needed.
    fn fetch_target_payload(
        &self,
        logical_path: &str,
        physical_path: Option<&str>,
    ) -> Result<Vec<u8>, ServiceError> {
        if let Some(bytes) = self.shared.uptane.target_file(logical_path)? {
            return Ok(bytes);
        }
        if let Some(physical) = physical_path {
            if let Some(bytes) = self.shared.uptane.target_file(physical)? {
                return Ok(bytes);
            }
        }
        Err(ServiceError::Uptane(UptaneError::MissingTargetPayload {
            path: logical_path.to_string(),
        }))
    }
}
