//! Bootstrap helpers for consumers embedding the remote-config service.
//!
//! This module exposes a thin wrapper that turns environment-derived settings into
//! a ready-to-use [`RemoteConfigService`]. It intentionally keeps the API surface
//! small so new agents can wire remote configuration with minimal boilerplate.

use std::borrow::Cow;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{error, warn};
use uuid::Uuid;

use crate::config::RemoteConfigEnv;
use crate::embedded_roots;
use crate::http::{Auth, HttpClient, HttpClientOptions};
use crate::rc_key::RcKey;
use crate::service::{RemoteConfigService, ServiceConfig, ServiceError};
use crate::store::RcStore;
use crate::telemetry::{CountingTelemetry, TelemetryCounters};
use crate::uptane::{UptaneConfig, UptaneState};

/// Error surfaced when bootstrap prerequisites are not met.
#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    /// Remote configuration is disabled by configuration or environment.
    #[error("remote configuration disabled")]
    Disabled,
    /// Missing API key prevents the HTTP client from authenticating.
    #[error("missing api key for remote configuration")]
    MissingApiKey,
    /// Failed to initialise the sled store.
    #[error("store error: {0}")]
    Store(#[from] crate::store::StoreError),
    /// Failed to construct the HTTP client.
    #[error("http client error: {0}")]
    Http(#[from] crate::http::HttpError),
    /// Remote configuration key could not be parsed.
    #[error("invalid rc key: {0}")]
    RcKey(#[from] crate::rc_key::RcKeyError),
    /// Failed to create the Uptane state manager.
    #[error("uptane error: {0}")]
    Uptane(#[from] crate::uptane::UptaneError),
    /// TLS requirements were violated.
    #[error("remote-config base url {0} requires DD_REMOTE_CONFIGURATION_NO_TLS=true")]
    InsecureTransport(String),
    /// Embedded trust anchors failed validation.
    #[error("embedded root error: {0}")]
    EmbeddedRoot(String),
}

/// Convenience result alias used by the bootstrap module.
pub type Result<T> = std::result::Result<T, BootstrapError>;

/// Material required to construct a [`RemoteConfigService`].
pub struct RemoteConfigBootstrap<'a> {
    /// Environment-derived configuration (enablement toggle, credentials, site).
    pub env: RemoteConfigEnv,
    /// Datadog agent version string reported to the backend.
    pub agent_version: &'a str,
    /// Optional agent UUID; generated when omitted to match Go behaviour.
    pub agent_uuid: Option<&'a str>,
    /// Optional tags attached to refresh requests.
    pub tags: Vec<String>,
    /// Store mode selection (persistent path or in-memory).
    pub store: StoreMode<'a>,
    /// Optional runtime provider mirroring the live agent configuration.
    pub runtime: Option<&'a dyn RemoteConfigRuntime>,
}

impl fmt::Debug for RemoteConfigBootstrap<'_> {
    /// Prints a redacted snapshot of the bootstrap input for troubleshooting.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteConfigBootstrap")
            .field("env", &self.env)
            .field("agent_version", &self.agent_version)
            .field("agent_uuid", &self.agent_uuid)
            .field("tags", &self.tags)
            .field("store", &self.store)
            .field(
                "runtime",
                &self.runtime.as_ref().map(|_| "RemoteConfigRuntime"),
            )
            .finish()
    }
}

/// Storage strategy used during bootstrap.
#[derive(Debug, Clone, Copy)]
pub enum StoreMode<'a> {
    /// Persist data on disk at the provided path.
    Persistent(&'a Path),
    /// Keep all state in memory; data is lost upon shutdown.
    Ephemeral,
}

impl<'a> RemoteConfigBootstrap<'a> {
    /// Builds the remote-config service, returning both the service and a telemetry handle.
    pub fn build(self) -> Result<BootstrapArtifacts> {
        // Honor enablement/force-off toggles before performing any IO.
        if !self.env.enabled {
            return Err(BootstrapError::Disabled);
        }

        let runtime_provider = self.runtime;
        let runtime_snapshot = runtime_provider
            .map(RemoteConfigRuntime::snapshot)
            .unwrap_or_default();
        let runtime_watch = runtime_provider.and_then(RemoteConfigRuntime::watch);
        let runtime_credentials = runtime_provider.and_then(RemoteConfigRuntime::credentials);
        let credential_watch = runtime_provider.and_then(RemoteConfigRuntime::credentials_watch);

        let api_key = runtime_credentials
            .as_ref()
            .map(|snapshot| snapshot.api_key.clone())
            .or_else(|| self.env.api_key.clone())
            // Propagate a dedicated error when credentials are missing.
            .ok_or(BootstrapError::MissingApiKey)?;
        let rc_key_value = runtime_credentials
            .as_ref()
            .and_then(|snapshot| snapshot.rc_key.clone())
            .or_else(|| self.env.rc_key.clone());
        let par_jwt_value = runtime_credentials
            .as_ref()
            .and_then(|snapshot| snapshot.par_jwt.clone())
            .or_else(|| self.env.par_jwt.clone());

        let mut runtime_values = RuntimeResolved::from_snapshot(&runtime_snapshot);
        if !self.tags.is_empty() {
            runtime_values = runtime_values.with_tags_override(self.tags);
        }
        let explicit_agent_uuid = self.agent_uuid.map(str::to_owned);
        runtime_values = runtime_values.override_agent_uuid(explicit_agent_uuid.as_deref());
        let (hostname, tags, trace_agent_env, runtime_agent_uuid) = runtime_values.into_parts();
        let agent_uuid = runtime_agent_uuid.unwrap_or_else(|| load_or_generate_uuid(self.store));

        let base_url = self.env.base_url();
        if !self.env.no_tls && base_url.starts_with("http://") {
            return Err(BootstrapError::InsecureTransport(base_url));
        }

        let decoded_rc_key = rc_key_value.as_deref().map(RcKey::decode).transpose()?;

        // Create the persistent store and Uptane state manager.
        let store = match self.store {
            StoreMode::Persistent(path) => {
                RcStore::open(path, self.agent_version, api_key.as_str(), &base_url)?
            }
            StoreMode::Ephemeral => {
                RcStore::open_ephemeral(self.agent_version, api_key.as_str(), &base_url)?
            }
        };

        let mut config = ServiceConfig {
            hostname,
            agent_version: self.agent_version.to_owned(),
            agent_uuid,
            tags,
            trace_agent_env,
            site: self.env.site.clone(),
            config_root_override: self.env.config_root_override.clone(),
            director_root_override: self.env.director_root_override.clone(),
            rc_key: decoded_rc_key.clone(),
            ..Default::default()
        };

        if let Some(value) = runtime_snapshot.default_refresh_interval {
            config.default_refresh_interval = value;
        }
        if let Some(value) = runtime_snapshot.min_refresh_interval {
            config.min_refresh_interval = value;
        }
        if let Some(value) = runtime_snapshot.org_status_interval {
            config.org_status_interval = value;
        }
        if let Some(value) = runtime_snapshot.clients_ttl {
            config.clients_ttl = value;
        }
        if let Some(value) = runtime_snapshot.cache_bypass_limit {
            config.cache_bypass_limit = value;
        }

        let mut uptane_config = UptaneConfig::default();
        uptane_config.org_binding.expected_org_id = config.rc_key.as_ref().map(|key| key.org_id);
        let uptane = UptaneState::with_config(store, uptane_config)?;

        seed_trust_roots(&uptane, &config)?;
        let runtime_updates = runtime_watch.map(RuntimeUpdates::new);

        // Build the HTTP client with sanitized headers.
        let auth = Auth {
            api_key: api_key.clone(),
            application_key: decoded_rc_key.as_ref().map(|key| key.app_key.clone()),
            par_jwt: par_jwt_value.clone(),
        };
        let http = HttpClient::new(
            base_url.clone(),
            &auth,
            &self.agent_version,
            HttpClientOptions {
                allow_plaintext: self.env.no_tls,
                accept_invalid_certs: self.env.no_tls_validation,
            },
        )?;

        let counters = Arc::new(TelemetryCounters::default());
        let telemetry = CountingTelemetry::new(counters.clone());
        let service = RemoteConfigService::new(http, uptane, config);

        let applied_credentials =
            CredentialSnapshot::new(api_key.clone(), rc_key_value.clone(), par_jwt_value.clone());
        let credential_updates = credential_watch
            .map(CredentialUpdates::new)
            .map(|updates| PendingCredentialUpdates::new(updates, applied_credentials.clone()));

        Ok(BootstrapArtifacts {
            service,
            telemetry,
            counters,
            runtime_updates,
            credential_updates,
        })
    }
}

/// Seeds the Uptane store with site-appropriate config and director roots.
fn seed_trust_roots(uptane: &UptaneState, config: &ServiceConfig) -> Result<()> {
    let (config_root, director_root) = embedded_roots::resolve(
        &config.site,
        config.config_root_override.as_deref(),
        config.director_root_override.as_deref(),
    )
    .map_err(|err| BootstrapError::EmbeddedRoot(err.to_string()))?;
    uptane
        .seed_trust_roots(&config_root.raw, &director_root.raw)
        .map_err(BootstrapError::Uptane)?;
    Ok(())
}

/// Attempts to load a persisted host UUID, falling back to auto-generation when unavailable.
fn load_or_generate_uuid(store_mode: StoreMode<'_>) -> String {
    match store_mode {
        StoreMode::Persistent(path) => {
            let uuid_dir = resolve_uuid_directory(path);
            if let Err(err) = fs::create_dir_all(&uuid_dir) {
                warn!(
                    error = %err,
                    directory = %uuid_dir.display(),
                    "remote-config: failed to prepare uuid directory"
                );
                return Uuid::new_v4().to_string();
            }
            let uuid_path = uuid_dir.join("remote-config.uuid");
            match fs::read_to_string(&uuid_path) {
                Ok(contents) => contents.trim().to_string(),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    let generated = Uuid::new_v4().to_string();
                    if let Err(write_err) = fs::write(&uuid_path, &generated) {
                        warn!(
                            error = %write_err,
                            path = %uuid_path.display(),
                            "remote-config: failed to persist host uuid"
                        );
                    }
                    generated
                }
                Err(err) => {
                    warn!(
                        error = %err,
                        path = %uuid_path.display(),
                        "remote-config: failed to load host uuid"
                    );
                    Uuid::new_v4().to_string()
                }
            }
        }
        StoreMode::Ephemeral => Uuid::new_v4().to_string(),
    }
}

/// Resolves the directory used to persist the UUID file.
fn resolve_uuid_directory(path: &Path) -> PathBuf {
    match fs::metadata(path) {
        Ok(metadata) => {
            if metadata.is_dir() {
                path.to_path_buf()
            } else {
                path.parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."))
            }
        }
        Err(err) => {
            if err.kind() == std::io::ErrorKind::NotFound {
                path.parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."))
            } else {
                warn!(
                    error = %err,
                    probe_path = %path.display(),
                    "remote-config: failed to inspect store path metadata"
                );
                path.parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."))
            }
        }
    }
}

/// Trait that allows hosts to project their live configuration (hostname, tags, refresh knobs)
/// into the remote-config bootstrap process.
pub trait RemoteConfigRuntime: Send + Sync {
    /// Returns a snapshot of the current runtime values.
    fn snapshot(&self) -> HostRuntimeSnapshot;
    /// Returns a watch channel that emits runtime snapshots when host config changes.
    fn watch(&self) -> Option<RuntimeWatch> {
        None
    }
    /// Returns the current credential snapshot (API key, RC key, PAR JWT) if available.
    fn credentials(&self) -> Option<CredentialSnapshot> {
        None
    }
    /// Returns a watch channel emitting credential updates.
    fn credentials_watch(&self) -> Option<CredentialWatch> {
        None
    }
}

/// Helper implementation of [`RemoteConfigRuntime`] backed by `tokio::sync::watch`
/// channels.  Embedder code can forward their own configuration/secret watchers
/// into this struct instead of implementing the trait manually.
///
/// # Example
/// ```no_run
/// use remote_config_core::bootstrap::{
///     CredentialSnapshot, HostRuntimeSnapshot, RemoteConfigBootstrap, StoreMode,
///     WatchedRemoteConfigRuntime,
/// };
/// use tokio::sync::watch;
///
/// let (_runtime_tx, runtime_rx) = watch::channel(HostRuntimeSnapshot::default());
/// let (_cred_tx, cred_rx) =
///     watch::channel(CredentialSnapshot::new("api_key".into(), None, None));
///
/// // Forward runtime + credential updates into the bootstrapper.
/// let runtime = WatchedRemoteConfigRuntime::new(HostRuntimeSnapshot::default())
///     .with_runtime_watch(runtime_rx)
///     .with_credentials(CredentialSnapshot::new("api_key".into(), None, None))
///     .with_credential_watch(cred_rx);
///
/// let bootstrap = RemoteConfigBootstrap {
///     env: remote_config_core::config::RemoteConfigEnv::from_os_env(),
///     agent_version: "1.0.0",
///     agent_uuid: None,
///     tags: Vec::new(),
///     store: StoreMode::Ephemeral,
///     runtime: Some(&runtime),
/// };
/// let _ = bootstrap.build();
/// ```
pub struct WatchedRemoteConfigRuntime {
    snapshot: HostRuntimeSnapshot,
    runtime_watch: Option<watch::Receiver<HostRuntimeSnapshot>>,
    credentials: Option<CredentialSnapshot>,
    credential_watch: Option<watch::Receiver<CredentialSnapshot>>,
}

impl fmt::Debug for WatchedRemoteConfigRuntime {
    /// Prints a concise summary of the installed watchers and snapshots.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WatchedRemoteConfigRuntime")
            .field("snapshot", &self.snapshot)
            .field("runtime_watch", &self.runtime_watch.is_some())
            .field("credentials", &self.credentials.is_some())
            .field("credential_watch", &self.credential_watch.is_some())
            .finish()
    }
}

impl WatchedRemoteConfigRuntime {
    /// Creates a runtime provider seeded with the supplied snapshot.
    pub fn new(snapshot: HostRuntimeSnapshot) -> Self {
        Self {
            snapshot,
            runtime_watch: None,
            credentials: None,
            credential_watch: None,
        }
    }

    /// Installs a `tokio::sync::watch` receiver that emits runtime updates.
    pub fn with_runtime_watch(mut self, receiver: watch::Receiver<HostRuntimeSnapshot>) -> Self {
        self.runtime_watch = Some(receiver);
        self
    }

    /// Provides an explicit credential snapshot (API key, RC key, PAR JWT).
    pub fn with_credentials(mut self, snapshot: CredentialSnapshot) -> Self {
        self.credentials = Some(snapshot);
        self
    }

    /// Installs a credential watcher so the service can rotate API/app keys dynamically.
    pub fn with_credential_watch(mut self, receiver: watch::Receiver<CredentialSnapshot>) -> Self {
        self.credential_watch = Some(receiver);
        self
    }
}

impl RemoteConfigRuntime for WatchedRemoteConfigRuntime {
    /// Returns the current runtime snapshot presented during bootstrap.
    fn snapshot(&self) -> HostRuntimeSnapshot {
        self.snapshot.clone()
    }

    /// Exposes a watch channel delivering runtime updates, when configured.
    fn watch(&self) -> Option<RuntimeWatch> {
        self.runtime_watch
            .as_ref()
            .map(|receiver| RuntimeWatch::new(receiver.clone()))
    }

    /// Returns the latest credential bundle, if any.
    fn credentials(&self) -> Option<CredentialSnapshot> {
        self.credentials.clone()
    }

    /// Exposes a credential watch channel so the service can rotate secrets.
    fn credentials_watch(&self) -> Option<CredentialWatch> {
        self.credential_watch
            .as_ref()
            .map(|receiver| CredentialWatch::new(receiver.clone()))
    }
}

/// Wrapper containing a `tokio::sync::watch::Receiver` for runtime snapshots.
pub struct RuntimeWatch {
    receiver: watch::Receiver<HostRuntimeSnapshot>,
}

impl RuntimeWatch {
    /// Creates a new runtime watch from the provided receiver.
    pub fn new(receiver: watch::Receiver<HostRuntimeSnapshot>) -> Self {
        Self { receiver }
    }

    /// Consumes the wrapper and returns the underlying receiver.
    fn into_inner(self) -> watch::Receiver<HostRuntimeSnapshot> {
        self.receiver
    }
}

/// Snapshot describing the authentication material required by Remote Config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialSnapshot {
    pub api_key: String,
    pub rc_key: Option<String>,
    pub par_jwt: Option<String>,
}

impl CredentialSnapshot {
    /// Builds a credential snapshot from API key, RC key, and PAR JWT parts.
    pub fn new(api_key: String, rc_key: Option<String>, par_jwt: Option<String>) -> Self {
        Self {
            api_key,
            rc_key,
            par_jwt,
        }
    }
}

/// Wrapper around a credential watch channel.
pub struct CredentialWatch {
    receiver: watch::Receiver<CredentialSnapshot>,
}

impl CredentialWatch {
    /// Wraps an existing credential watch receiver.
    pub fn new(receiver: watch::Receiver<CredentialSnapshot>) -> Self {
        Self { receiver }
    }

    /// Consumes the wrapper and returns the raw watch receiver.
    fn into_inner(self) -> watch::Receiver<CredentialSnapshot> {
        self.receiver
    }
}

/// Point-in-time configuration snapshot provided by [`RemoteConfigRuntime`].
#[derive(Debug, Clone, Default)]
pub struct HostRuntimeSnapshot {
    /// Hostname reported to the backend.
    pub hostname: Option<Cow<'static, str>>,
    /// Tags propagated with refresh requests.
    pub tags: Option<Cow<'static, [String]>>,
    /// Trace agent environment string.
    pub trace_agent_env: Option<Cow<'static, str>>,
    /// Agent UUID override (if the host already tracks one).
    pub agent_uuid: Option<Cow<'static, str>>,
    /// Default refresh cadence recommendation.
    pub default_refresh_interval: Option<Duration>,
    /// Minimum refresh interval clamp.
    pub min_refresh_interval: Option<Duration>,
    /// Organisation status polling cadence.
    pub org_status_interval: Option<Duration>,
    /// TTL applied to registered clients.
    pub clients_ttl: Option<Duration>,
    /// Cache-bypass limit per refresh window.
    pub cache_bypass_limit: Option<usize>,
}

/// Resolved runtime fields used to seed `ServiceConfig`.
#[derive(Debug, Default)]
struct RuntimeResolved {
    hostname: String,
    tags: Vec<String>,
    trace_agent_env: Option<String>,
    agent_uuid: Option<String>,
}

/// Clones a borrowed-or-owned slice of tags into an owned `Vec<String>`.
fn clone_tag_slice(slice: &Cow<'static, [String]>) -> Vec<String> {
    match slice {
        // Borrowed slices live in static storage, so we copy them into an owned buffer.
        Cow::Borrowed(values) => values.to_vec(),
        // Owned slices can be reused directly without additional allocations.
        Cow::Owned(values) => values.clone(),
    }
}

impl RuntimeResolved {
    /// Builds a resolved snapshot from the host-provided runtime data.
    fn from_snapshot(snapshot: &HostRuntimeSnapshot) -> Self {
        let hostname = snapshot
            .hostname
            .as_ref()
            .map(|value| value.clone().into_owned())
            .unwrap_or_else(String::new);
        let tags = snapshot
            .tags
            .as_ref()
            .map(clone_tag_slice)
            .unwrap_or_default();
        let trace_agent_env = snapshot
            .trace_agent_env
            .as_ref()
            .map(|value| value.clone().into_owned());
        let agent_uuid = snapshot
            .agent_uuid
            .as_ref()
            .map(|value| value.clone().into_owned());
        Self {
            hostname,
            tags,
            trace_agent_env,
            agent_uuid,
        }
    }

    /// Overrides the resolved tags when callers provide explicit values.
    ///
    /// # Examples
    /// ```ignore
    /// let resolved = RuntimeResolved::default()
    ///     .with_tags_override(["env:prod", "service:web"]);
    /// ```
    fn with_tags_override<I>(mut self, tags: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        let collected: Vec<String> = tags.into_iter().map(Into::into).collect();
        if !collected.is_empty() {
            self.tags = collected;
        }
        self
    }

    /// Overrides the agent UUID when a higher-precedence source supplies it.
    fn override_agent_uuid(mut self, uuid: Option<&str>) -> Self {
        if let Some(value) = uuid {
            self.agent_uuid = Some(value.to_owned());
        }
        self
    }

    /// Deconstructs the resolved values into the pieces required by bootstrap.
    fn into_parts(self) -> (String, Vec<String>, Option<String>, Option<String>) {
        (
            self.hostname,
            self.tags,
            self.trace_agent_env,
            self.agent_uuid,
        )
    }
}

/// Background runtime updates stream wiring host metadata changes into the service.
#[derive(Debug)]
struct RuntimeUpdates {
    receiver: watch::Receiver<HostRuntimeSnapshot>,
}

impl RuntimeUpdates {
    /// Wraps a runtime watch channel so changes can be propagated to the service.
    fn new(watch: RuntimeWatch) -> Self {
        Self {
            receiver: watch.into_inner(),
        }
    }

    /// Forwards runtime snapshot changes to the service until the watch channel closes.
    async fn spawn(self, service: RemoteConfigService) {
        let weak = service.downgrade();
        let mut receiver = self.receiver;
        let handle = tokio::spawn(async move {
            // Apply the current snapshot immediately if the service is still alive.
            if let Some(shared) = weak.upgrade() {
                let service = RemoteConfigService::from_shared(shared);
                let initial = receiver.borrow().clone();
                service.apply_runtime_snapshot(initial).await;
            } else {
                return;
            }
            while receiver.changed().await.is_ok() {
                if let Some(shared) = weak.upgrade() {
                    let service = RemoteConfigService::from_shared(shared);
                    let snapshot = receiver.borrow().clone();
                    service.apply_runtime_snapshot(snapshot).await;
                } else {
                    break;
                }
            }
        });
        service.register_aux_task(handle).await;
    }
}

struct CredentialUpdates {
    receiver: watch::Receiver<CredentialSnapshot>,
}

impl CredentialUpdates {
    /// Wraps a credential watch channel so API/RC key changes can be replayed.
    fn new(watch: CredentialWatch) -> Self {
        Self {
            receiver: watch.into_inner(),
        }
    }

    /// Rotates credentials whenever the watch channel emits a new snapshot,
    /// ignoring duplicate updates to avoid unnecessary HTTP header churn.
    async fn spawn(self, service: RemoteConfigService, baseline: CredentialSnapshot) {
        let weak = service.downgrade();
        let mut receiver = self.receiver;
        let handle = tokio::spawn(async move {
            let mut previous = baseline;
            while receiver.changed().await.is_ok() {
                let snapshot = receiver.borrow().clone();
                if snapshot == previous {
                    continue;
                }
                let Some(shared) = weak.upgrade() else {
                    break;
                };
                let service = RemoteConfigService::from_shared(shared);
                if let Err(err) = service
                    .rotate_credentials(
                        &snapshot.api_key,
                        snapshot.rc_key.as_deref(),
                        snapshot.par_jwt.as_deref(),
                    )
                    .await
                {
                    error!("remote-config credential rotation failed: {err}");
                } else {
                    previous = snapshot;
                }
            }
        });
        service.register_aux_task(handle).await;
    }
}

struct PendingCredentialUpdates {
    updates: CredentialUpdates,
    baseline: CredentialSnapshot,
}

impl PendingCredentialUpdates {
    /// Captures the update stream plus the last applied credentials.
    fn new(updates: CredentialUpdates, baseline: CredentialSnapshot) -> Self {
        Self { updates, baseline }
    }

    /// Starts the credential watcher.
    async fn spawn(self, service: RemoteConfigService) {
        self.updates.spawn(service, self.baseline).await;
    }
}

/// Convenience bundle returned by [`RemoteConfigBootstrap::build`].
pub struct BootstrapArtifacts {
    /// Fully initialised remote-config service.
    pub service: RemoteConfigService,
    /// Counter-backed telemetry reporter ready to be installed.
    pub telemetry: CountingTelemetry,
    /// Shared telemetry counters to expose to embedder metrics pipelines.
    pub counters: Arc<TelemetryCounters>,
    /// Optional runtime update stream that keeps host metadata fresh.
    runtime_updates: Option<RuntimeUpdates>,
    /// Optional credential update stream that rotates API/app keys when notified.
    credential_updates: Option<PendingCredentialUpdates>,
}

impl BootstrapArtifacts {
    /// Attaches the telemetry reporter to the service and returns the service reference.
    pub async fn install(self) -> (RemoteConfigService, Arc<TelemetryCounters>) {
        let Self {
            service,
            telemetry,
            counters,
            runtime_updates,
            credential_updates,
        } = self;
        service.set_telemetry(Arc::new(telemetry)).await;
        if let Some(updates) = runtime_updates {
            updates.spawn(service.clone()).await;
        }
        if let Some(pending) = credential_updates {
            pending.spawn(service.clone()).await;
        }
        (service, counters)
    }
}

impl fmt::Debug for BootstrapArtifacts {
    /// Prints a minimal summary without exposing sensitive internals.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BootstrapArtifacts")
            .field("service", &"<RemoteConfigService>")
            .field("telemetry", &self.telemetry)
            .field("counters", &self.counters)
            .finish()
    }
}

/// Trait implemented by hosts exposing remote-config capabilities.
pub trait RemoteConfigHost {
    /// Returns a reference to the underlying remote-config service.
    fn remote_config_service(&self) -> &RemoteConfigService;
}

/// Helper that wraps [`RemoteConfigService::client_get_configs`] for host implementers.
pub async fn handle_client_request(
    host: &impl RemoteConfigHost,
    request: remote_config_proto::remoteconfig::ClientGetConfigsRequest,
) -> std::result::Result<remote_config_proto::remoteconfig::ClientGetConfigsResponse, ServiceError>
{
    host.remote_config_service()
        .client_get_configs(request)
        .await
}

/// Helper that accepts raw protobuf bytes and returns an encoded response.
pub async fn handle_client_request_bytes(
    host: &impl RemoteConfigHost,
    payload: &[u8],
) -> std::result::Result<Vec<u8>, ServiceError> {
    host.remote_config_service()
        .client_get_configs_from_bytes(payload)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RemoteConfigEnv;
    use crate::service::test_support::{base_config, build_service_with_config, sample_response};
    use httptest::matchers::{all_of, contains, request};
    use httptest::{responders::status_code, Expectation, Server};
    use prost::Message;
    use remote_config_proto::remoteconfig::{ClientGetConfigsRequest, OrgDataResponse};
    use std::fs;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::watch;

    /// Builds an enabled environment configuration for tests that need credentials.
    fn env_enabled(api_key: &str) -> RemoteConfigEnv {
        RemoteConfigEnv {
            enabled: true,
            api_key: Some(api_key.to_string()),
            rc_key: None,
            par_jwt: None,
            config_root_override: None,
            director_root_override: None,
            site: "datadoghq.com".into(),
            no_tls: false,
            no_tls_validation: false,
        }
    }

    #[test]
    /// Ensures bootstrap returns `Disabled` when the environment turns the feature off.
    fn bootstrap_disabled_returns_error() {
        let env = RemoteConfigEnv {
            enabled: false,
            api_key: Some("key".into()),
            rc_key: None,
            par_jwt: None,
            config_root_override: None,
            director_root_override: None,
            site: "datadoghq.com".into(),
            no_tls: false,
            no_tls_validation: false,
        };
        let tmp = TempDir::new().unwrap();
        let bootstrap = RemoteConfigBootstrap {
            env,
            agent_version: "1.0.0",
            agent_uuid: Some("uuid"),
            tags: Vec::new(),
            store: StoreMode::Persistent(tmp.path()),
            runtime: None,
        };
        let err = bootstrap.build().expect_err("bootstrap should fail");
        assert!(matches!(err, BootstrapError::Disabled));
    }

    #[test]
    /// Verifies bootstrap aborts when no API key material is available.
    fn bootstrap_missing_api_key_errors() {
        let env = RemoteConfigEnv {
            enabled: true,
            api_key: None,
            rc_key: None,
            par_jwt: None,
            config_root_override: None,
            director_root_override: None,
            site: "datadoghq.com".into(),
            no_tls: false,
            no_tls_validation: false,
        };
        let tmp = TempDir::new().unwrap();
        let bootstrap = RemoteConfigBootstrap {
            env,
            agent_version: "1.0.0",
            agent_uuid: Some("uuid"),
            tags: Vec::new(),
            store: StoreMode::Persistent(tmp.path()),
            runtime: None,
        };
        let err = bootstrap.build().expect_err("bootstrap should fail");
        assert!(matches!(err, BootstrapError::MissingApiKey));
    }

    #[tokio::test]
    /// Confirms a successful bootstrap configures the service and telemetry counters.
    async fn bootstrap_success_sets_expected_fields() {
        let env = env_enabled("api_key");
        let tmp = TempDir::new().unwrap();
        let bootstrap = RemoteConfigBootstrap {
            env,
            agent_version: "1.0.0",
            agent_uuid: Some("uuid"),
            tags: vec!["env:test".into()],
            store: StoreMode::Persistent(tmp.path()),
            runtime: None,
        };

        let artifacts = bootstrap.build().expect("bootstrap succeeds");
        let (service, counters) = artifacts.install().await;
        assert_eq!(service.config().agent_uuid, "uuid");
        assert!(service.snapshot().await.first_refresh_pending);
        assert_eq!(counters.snapshot().refresh_success, 0);
    }

    #[tokio::test]
    /// Generates a UUID on behalf of hosts that did not supply one.
    async fn bootstrap_generates_uuid_when_missing() {
        let env = env_enabled("api_key");
        let tmp = TempDir::new().unwrap();
        let bootstrap = RemoteConfigBootstrap {
            env,
            agent_version: "1.0.0",
            agent_uuid: None,
            tags: Vec::new(),
            store: StoreMode::Persistent(tmp.path()),
            runtime: None,
        };

        let artifacts = bootstrap.build().expect("bootstrap succeeds");
        let (service, _) = artifacts.install().await;
        assert!(!service.config().agent_uuid.is_empty());
    }

    #[tokio::test]
    /// Validates that ephemeral stores still bootstrap correctly and mark pending refreshes.
    async fn bootstrap_supports_ephemeral_store() {
        let env = env_enabled("api_key");
        let bootstrap = RemoteConfigBootstrap {
            env,
            agent_version: "1.0.0",
            agent_uuid: None,
            tags: Vec::new(),
            store: StoreMode::Ephemeral,
            runtime: None,
        };

        let artifacts = bootstrap.build().expect("bootstrap succeeds");
        let (service, _) = artifacts.install().await;
        assert!(service.snapshot().await.first_refresh_pending);
        assert!(service.config().agent_uuid.len() > 0);
    }

    #[test]
    /// Applies runtime snapshot overrides to the service configuration.
    fn bootstrap_applies_runtime_snapshot_overrides() {
        let runtime = WatchedRemoteConfigRuntime::new(HostRuntimeSnapshot {
            hostname: Some("runtime-host".into()),
            tags: Some(vec!["env:runtime".into()].into()),
            trace_agent_env: Some("trace-env".into()),
            agent_uuid: Some("runtime-uuid".into()),
            default_refresh_interval: Some(Duration::from_secs(42)),
            min_refresh_interval: Some(Duration::from_secs(7)),
            org_status_interval: Some(Duration::from_secs(55)),
            clients_ttl: Some(Duration::from_secs(33)),
            cache_bypass_limit: Some(7),
        });
        let env = env_enabled("api_key");
        let bootstrap = RemoteConfigBootstrap {
            env,
            agent_version: "7.7.7",
            agent_uuid: None,
            tags: Vec::new(),
            store: StoreMode::Ephemeral,
            runtime: Some(&runtime),
        };

        let service = bootstrap.build().expect("bootstrap succeeds").service;
        let config = service.config().clone();
        assert_eq!(config.hostname, "runtime-host");
        assert_eq!(config.tags, vec!["env:runtime"]);
        assert_eq!(config.trace_agent_env.as_deref(), Some("trace-env"));
        assert_eq!(config.agent_uuid, "runtime-uuid");
        assert_eq!(config.default_refresh_interval, Duration::from_secs(42));
        assert_eq!(config.min_refresh_interval, Duration::from_secs(7));
        assert_eq!(config.org_status_interval, Duration::from_secs(55));
        assert_eq!(config.clients_ttl, Duration::from_secs(33));
        assert_eq!(config.cache_bypass_limit, 7);
    }

    #[test]
    /// Ensures explicit bootstrap inputs win over runtime-provided values.
    fn bootstrap_prefers_explicit_inputs_over_runtime_snapshot() {
        let runtime = WatchedRemoteConfigRuntime::new(HostRuntimeSnapshot {
            agent_uuid: Some("runtime-uuid".into()),
            tags: Some(vec!["env:runtime".into()].into()),
            ..Default::default()
        });
        let env = env_enabled("api_key");
        let bootstrap = RemoteConfigBootstrap {
            env,
            agent_version: "1.0.0",
            agent_uuid: Some("explicit-uuid"),
            tags: vec!["env:explicit".into()],
            store: StoreMode::Ephemeral,
            runtime: Some(&runtime),
        };

        let service = bootstrap.build().expect("bootstrap succeeds").service;
        let config = service.config().clone();
        assert_eq!(config.agent_uuid, "explicit-uuid");
        assert_eq!(config.tags, vec!["env:explicit"]);
    }

    #[tokio::test]
    /// Propagates runtime watcher updates into the live service state.
    async fn runtime_watch_updates_are_applied() {
        let env = env_enabled("api_key");
        let initial = HostRuntimeSnapshot {
            hostname: Some("runtime-host".into()),
            tags: Some(vec!["env:runtime".into()].into()),
            trace_agent_env: Some("trace-env".into()),
            ..Default::default()
        };
        let (tx, rx) = watch::channel(initial.clone());
        let runtime = WatchedRemoteConfigRuntime::new(initial.clone()).with_runtime_watch(rx);
        let bootstrap = RemoteConfigBootstrap {
            env,
            agent_version: "1.0.0",
            agent_uuid: None,
            tags: Vec::new(),
            store: StoreMode::Ephemeral,
            runtime: Some(&runtime),
        };

        let (service, _) = bootstrap
            .build()
            .expect("bootstrap succeeds")
            .install()
            .await;
        // Ensure initial snapshot applied.
        let state = service.runtime_state().await;
        assert_eq!(state.hostname, "runtime-host");
        assert_eq!(state.tags, vec!["env:runtime"]);
        assert_eq!(state.trace_agent_env.as_deref(), Some("trace-env"));

        let mut updated = initial.clone();
        updated.hostname = Some("updated-host".into());
        updated.tags = Some(vec!["env:updated".into()].into());
        updated.trace_agent_env = Some("trace-updated".into());
        tx.send(updated).expect("send runtime update");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let snapshot = service.runtime_state().await;
                if snapshot.hostname == "updated-host"
                    && snapshot.tags == vec!["env:updated"]
                    && snapshot.trace_agent_env.as_deref() == Some("trace-updated")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("runtime metadata should update");
    }

    #[tokio::test(flavor = "multi_thread")]
    /// Ensures credential watch notifications rotate HTTPS headers and trigger refreshes.
    async fn credential_watch_triggers_rotation() {
        let server = Server::run();
        let org_response = OrgDataResponse {
            uuid: "updated-org".into(),
            ..Default::default()
        };
        server.expect(
            Expectation::matching(request::method_path("GET", "/api/v0.1/org"))
                .times(1)
                .respond_with(status_code(200).body(org_response.encode_to_vec())),
        );

        // Expect the next refresh to use the rotated API key.
        let refresh_response = sample_response(1, 1);
        server.expect(
            Expectation::matching(all_of![
                request::method_path("POST", "/api/v0.1/configurations"),
                request::headers(contains(("dd-api-key", "updated-key")))
            ])
            .times(1)
            .respond_with(status_code(200).body(refresh_response.encode_to_vec())),
        );

        let mut config = base_config();
        config.disable_background_poller = true;
        config.enable_websocket_echo = false;
        let service = build_service_with_config(&server, config);

        let baseline = CredentialSnapshot::new("key".into(), None, None);
        let (tx, rx) = watch::channel(baseline.clone());
        let updates = CredentialUpdates::new(CredentialWatch::new(rx));
        updates.spawn(service.clone(), baseline).await;

        let updated = CredentialSnapshot::new("updated-key".into(), None, None);
        tx.send(updated).expect("send credential update");

        tokio::time::sleep(Duration::from_millis(200)).await;
        service.refresh_once().await.expect("refresh succeeds");
    }

    #[test]
    /// Reads an existing UUID from disk instead of generating a new one.
    fn load_or_generate_uuid_reads_existing_value() {
        let tmp = TempDir::new().unwrap();
        let uuid_path = tmp.path().join("remote-config.uuid");
        fs::write(&uuid_path, " existing-uuid \n").unwrap();

        let uuid = load_or_generate_uuid(StoreMode::Persistent(tmp.path()));
        assert_eq!(uuid, "existing-uuid");
    }

    #[test]
    /// Creates parent directories when the store path does not exist yet.
    fn load_or_generate_uuid_creates_missing_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let nested_store = tmp.path().join("nonexistent").join("store.db");
        let uuid = load_or_generate_uuid(StoreMode::Persistent(&nested_store));
        assert!(!uuid.is_empty());
        let uuid_path = tmp.path().join("nonexistent").join("remote-config.uuid");
        assert!(
            uuid_path.exists(),
            "uuid file should be created in the derived directory"
        );
        let second = load_or_generate_uuid(StoreMode::Persistent(&nested_store));
        assert_eq!(uuid, second, "uuid should remain stable across calls");
    }

    #[test]
    /// Stores the UUID next to the sled database file instead of below the file path.
    fn load_or_generate_uuid_handles_file_paths() {
        let tmp = TempDir::new().unwrap();
        let store_file = tmp.path().join("remote-config.db");

        let first = load_or_generate_uuid(StoreMode::Persistent(&store_file));
        let uuid_path = tmp.path().join("remote-config.uuid");
        assert!(
            uuid_path.exists(),
            "uuid should be written in the parent directory of the store file"
        );
        let second = load_or_generate_uuid(StoreMode::Persistent(&store_file));
        assert_eq!(first, second, "uuid should remain stable for file paths");
    }

    #[test]
    /// Falls back to a generated UUID when reading fails unexpectedly.
    fn load_or_generate_uuid_handles_unexpected_read_errors() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("remote-config.uuid")).unwrap();
        let uuid = load_or_generate_uuid(StoreMode::Persistent(tmp.path()));
        assert!(!uuid.is_empty());
    }

    /// Minimal host implementation used to exercise helper functions.
    struct TestHost {
        service: RemoteConfigService,
        #[allow(dead_code)]
        _tmp: TempDir,
    }

    impl RemoteConfigHost for TestHost {
        /// Exposes the hosted service so helper utilities can reuse it.
        fn remote_config_service(&self) -> &RemoteConfigService {
            &self.service
        }
    }

    /// Builds a test host with a fully initialised service.
    async fn make_test_host() -> TestHost {
        let env = env_enabled("api_key");
        let tmp = TempDir::new().unwrap();
        let bootstrap = RemoteConfigBootstrap {
            env,
            agent_version: "1.0.0",
            agent_uuid: None,
            tags: Vec::new(),
            store: StoreMode::Persistent(tmp.path()),
            runtime: None,
        };
        let artifacts = bootstrap.build().expect("bootstrap succeeds");
        let (service, _) = artifacts.install().await;
        TestHost { service, _tmp: tmp }
    }

    #[tokio::test]
    /// Validates helper requests bubble up service-level validation errors.
    async fn handle_client_request_propagates_errors() {
        let host = make_test_host().await;
        let err = handle_client_request(&host, ClientGetConfigsRequest::default())
            .await
            .expect_err("request should fail validation");
        match err {
            ServiceError::InvalidRequest(message) => {
                assert!(
                    message.contains("client is a required field"),
                    "unexpected validation message: {message}"
                );
            }
            other => panic!("expected invalid request error, got {other:?}"),
        }
    }

    #[tokio::test]
    /// Ensures the byte-oriented helper surfaces protobuf decode failures.
    async fn handle_client_request_bytes_propagates_decode_errors() {
        let host = make_test_host().await;
        let payload = vec![0x01, 0x02, 0x03];
        let err = handle_client_request_bytes(&host, &payload)
            .await
            .expect_err("invalid payload should fail");
        match err {
            ServiceError::Protobuf(_) => {}
            other => panic!("expected protobuf error, got {other:?}"),
        }
    }
}
