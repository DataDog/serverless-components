//! Remote Configuration CDN client.
//!
//! The Go agent exposes a secondary client that speaks to the Remote Config
//! Content Delivery Network (CDN). Agentless workloads depend on this code
//! path to hydrate their local Uptane caches without talking to the agent
//! coordinator API. This module mirrors that behaviour so Rust consumers can
//! reach feature parity with the Go implementation.
//!
//! The client performs the following duties:
//! - Maintains a sled-backed Uptane cache dedicated to CDN updates.
//! - Fetches metadata and target files from the CDN with the same throttling
//!   window enforced by the Go code (50 seconds).
//! - Reconstructs a `LatestConfigsResponse` payload from the fetched artefacts
//!   and feeds it into the shared [`UptaneState`] helper so downstream code can
//!   reuse the existing storage and filtering logic.
//! - Surfaces a high-level [`CdnUpdate`] structure that mirrors
//!   `pkg/remoteconfig/state.Update` from the Go repository.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use datadog_fips::reqwest_adapter::create_reqwest_client_builder;
use remote_config_proto::remoteconfig::{
    Client, ConfigMetas, DelegatedMeta, DirectorMetas, File, LatestConfigsResponse, TopMeta,
};
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Client as HttpClient, StatusCode, Url};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::warn;

use crate::embedded_roots;
use crate::service::client::compute_matched_configs;
use crate::store::{RcStore, StoreError};
use crate::uptane::{TufVersions, UptaneError, UptaneState};

/// Name of the sled database created for CDN state when using the helper constructor.
const CDN_DATABASE_FILENAME: &str = "remote-config-cdn.db";
/// Header carrying the refreshed authorisation token returned by the CDN.
const CDN_REFRESHED_AUTH_HEADER: &str = "X-Dd-Refreshed-Authorization";
/// Header used to authenticate requests against the CDN.
const CDN_API_KEY_HEADER: &str = "X-Dd-Api-Key";
/// Default throttling window applied between successive CDN updates.
const DEFAULT_UPDATE_THROTTLE: Duration = Duration::from_secs(50);

/// Errors returned by the CDN client.
#[derive(Debug, Error)]
pub enum CdnError {
    /// The CDN rejected our credentials.
    #[error("unauthorized when fetching remote-config CDN artefacts")]
    Unauthorized,
    /// The requested resource was not found on the CDN.
    #[error("resource '{0}' was not found on the remote-config CDN")]
    NotFound(String),
    /// The CDN responded with an unexpected status code.
    #[error("unexpected CDN status {0}")]
    Status(u16),
    /// Networking or TLS errors bubbled up from the HTTP client.
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    /// Metadata payloads could not be parsed as JSON.
    #[error("failed to parse metadata payload: {0}")]
    Metadata(#[from] serde_json::Error),
    /// Metadata validation failed while filtering targets.
    #[error("metadata validation error: {0}")]
    Validation(String),
    /// Persisted store operations failed.
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    /// Uptane cache operations failed.
    #[error("uptane error: {0}")]
    Uptane(#[from] UptaneError),
    /// Target metadata contained invalid paths.
    #[error("invalid CDN base url: {0}")]
    InvalidBaseUrl(String),
    /// FIPS TLS configuration error.
    #[error("FIPS configuration error: {0}")]
    FipsConfig(String),
    /// Embedded trust anchors failed to load.
    #[error("embedded root error: {0}")]
    EmbeddedRoot(String),
}

/// High-level summary of an update returned by the CDN client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdnUpdate {
    /// New director roots that should be applied by the client.
    pub tuf_roots: Vec<Vec<u8>>,
    /// Latest director targets metadata.
    pub tuf_targets: Vec<u8>,
    /// Target files keyed by their TUF path.
    pub target_files: HashMap<String, Vec<u8>>,
    /// List of target paths applicable to the caller.
    pub client_configs: Vec<String>,
}

impl CdnUpdate {
    /// Indicates whether the update is empty (no change).
    pub fn is_empty(&self) -> bool {
        self.tuf_roots.is_empty()
            && self.tuf_targets.is_empty()
            && self.target_files.is_empty()
            && self.client_configs.is_empty()
    }
}

/// CDN client configuration values.
#[derive(Debug, Clone)]
pub struct CdnClientConfig {
    /// Datadog site (e.g. `https://app.datadoghq.com`).
    pub site: String,
    /// Datadog API key used to authenticate CDN requests.
    pub api_key: String,
    /// Datadog agent version reported within the store metadata.
    pub agent_version: String,
    /// Optional throttle override (defaults to the Go agent value of 50 seconds).
    pub throttle: Option<Duration>,
    /// Optional base URL override used by tests to point at stub servers.
    pub base_url_override: Option<String>,
    /// Optional override for the embedded config root metadata.
    pub config_root_override: Option<String>,
    /// Optional override for the embedded director root metadata.
    pub director_root_override: Option<String>,
}

impl Default for CdnClientConfig {
    /// Provides conservative defaults mirroring the Go agent runtime.
    fn default() -> Self {
        Self {
            site: "https://app.datadoghq.com".to_string(),
            api_key: String::new(),
            agent_version: String::new(),
            throttle: None,
            base_url_override: None,
            config_root_override: None,
            director_root_override: None,
        }
    }
}

/// Namespaces used to scope CDN authorisation tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CdnNamespace {
    Config,
    Director,
}

impl CdnNamespace {
    /// Infers the namespace (config vs director) from a CDN path.
    fn from_path(path: &str) -> Self {
        let trimmed = path.trim_start_matches('/');
        if trimmed.starts_with("config/") {
            CdnNamespace::Config
        } else {
            CdnNamespace::Director
        }
    }
}

/// Internal mutable state guarded by a mutex.
#[derive(Debug)]
struct CdnInnerState {
    /// Most recent timestamp when the CDN was polled.
    last_update: Option<Instant>,
    /// Temporary authorisation tokens refreshed by the CDN, scoped per namespace.
    auth_tokens: HashMap<CdnNamespace, String>,
}

impl Default for CdnInnerState {
    /// Initialises the throttling/auth state with empty values.
    fn default() -> Self {
        Self {
            last_update: None,
            auth_tokens: HashMap::new(),
        }
    }
}

/// Remote Configuration CDN client mirroring the Go agent implementation.
#[derive(Debug, Clone)]
pub struct CdnClient {
    /// Underlying HTTP client reused across requests.
    http: HttpClient,
    /// Base URL constructed for the CDN host.
    base_url: Url,
    /// Path prefix derived from the Datadog site (e.g. `datadoghq.com`).
    path_prefix: String,
    /// API key sent alongside CDN requests.
    api_key: String,
    /// Minimum duration between refresh attempts.
    throttle: Duration,
    /// Shared mutable state protecting throttling metadata and auth tokens.
    state: Arc<Mutex<CdnInnerState>>,
    /// Persisted Uptane cache backed by sled.
    uptane: UptaneState,
}

impl CdnClient {
    /// Opens (or creates) the CDN store below the provided `run_path` and constructs a client.
    pub fn with_run_path<P: AsRef<Path>>(
        run_path: P,
        config: CdnClientConfig,
    ) -> Result<Self, CdnError> {
        let run_path = run_path.as_ref();
        // Reject construction attempts that lack credentials so we never hit the CDN anonymously.
        if config.api_key.is_empty() {
            return Err(CdnError::Unauthorized);
        }
        let base_url = resolve_cdn_base_url(&config)?;
        let path_prefix = resolve_cdn_path_prefix(&config.site);
        let db_path = run_path.join(CDN_DATABASE_FILENAME);
        let store = RcStore::open(
            &db_path,
            &config.agent_version,
            &config.api_key,
            base_url.as_str(),
        )?;
        Self::with_store(store, config, base_url, path_prefix)
    }

    /// Builds a client using an already initialised [`RcStore`].
    pub fn with_store(
        store: RcStore,
        config: CdnClientConfig,
        base_url: Url,
        path_prefix: String,
    ) -> Result<Self, CdnError> {
        let throttle = config.throttle.unwrap_or(DEFAULT_UPDATE_THROTTLE);
        let http = create_reqwest_client_builder()
            .map_err(|e| CdnError::FipsConfig(e.to_string()))?
            .build()?;
        let uptane = UptaneState::new(store)?;
        let site_domain = resolve_site_domain(&config.site);
        let (config_root, director_root) = embedded_roots::resolve(
            &site_domain,
            config.config_root_override.as_deref(),
            config.director_root_override.as_deref(),
        )
        .map_err(|err| CdnError::EmbeddedRoot(err.to_string()))?;
        uptane.seed_trust_roots(&config_root.raw, &director_root.raw)?;

        Ok(Self {
            http,
            base_url,
            path_prefix,
            api_key: config.api_key,
            throttle,
            state: Arc::new(Mutex::new(CdnInnerState::default())),
            uptane,
        })
    }

    /// Returns a handle to the underlying Uptane state (useful for diagnostics).
    pub fn uptane_state(&self) -> UptaneState {
        self.uptane.clone()
    }

    /// Attempts to fetch a CDN update matching the provided product list and current Uptane state.
    pub async fn get_update(
        &self,
        products: &[String],
        current_targets_version: u64,
        current_root_version: u64,
    ) -> Result<Option<CdnUpdate>, CdnError> {
        // Only hit the CDN when the throttle window allows another refresh attempt.
        if self.should_update().await {
            if let Err(err) = self.refresh().await {
                // Refresh failures are non-fatal; log and fall back to the cached state.
                warn!(error = %err, "remote-config CDN refresh failed");
            }
        }

        let versions = self.uptane.tuf_versions()?;
        // Exit early when the director targets have not changed to avoid re-sending the same payload.
        if versions.director_targets == current_targets_version {
            return Ok(None);
        }

        let targets = self.uptane.director_targets()?;
        let matched = select_targets_for_products(products, &targets)?;

        let tuf_targets = self.uptane.director_targets_raw()?;
        let target_files = self.load_target_files(&matched)?;
        let tuf_roots = self.collect_new_roots(current_root_version, &versions)?;

        Ok(Some(CdnUpdate {
            tuf_roots,
            tuf_targets,
            target_files,
            client_configs: matched,
        }))
    }

    /// Determines whether the throttle window permits another CDN refresh.
    async fn should_update(&self) -> bool {
        let mut guard = self.state.lock().await;
        let now = Instant::now();
        let should_update = match guard.last_update {
            Some(previous) => {
                // Allow updates only when the throttle window has elapsed to
                // avoid hammering the CDN with redundant requests.
                now.duration_since(previous) >= self.throttle
            }
            None => true,
        };
        if should_update {
            guard.last_update = Some(now);
        }
        should_update
    }

    /// Fetches metadata and target files from the CDN and updates the local cache atomically.
    async fn refresh(&self) -> Result<(), CdnError> {
        // Pull the config repository metadata in the same order as the Go agent
        // so we can apply them atomically.
        let (config_root, config_root_version) = self.fetch_top_meta("config/root.json").await?;
        let (config_timestamp, config_timestamp_version) =
            self.fetch_top_meta("config/timestamp.json").await?;
        let (config_snapshot, config_snapshot_version) =
            self.fetch_top_meta("config/snapshot.json").await?;
        let (config_targets, config_targets_version) =
            self.fetch_top_meta("config/targets.json").await?;

        let delegated = self.fetch_delegated_metadata(&config_targets).await?;

        // Repeat for the director repository so target evaluation remains consistent.
        let (director_root, director_root_version) =
            self.fetch_top_meta("director/root.json").await?;
        let (director_timestamp, director_timestamp_version) =
            self.fetch_top_meta("director/timestamp.json").await?;
        let (director_snapshot, director_snapshot_version) =
            self.fetch_top_meta("director/snapshot.json").await?;
        let (director_targets, director_targets_version) =
            self.fetch_top_meta("director/targets.json").await?;

        let target_files = self.fetch_target_files(&director_targets).await?;

        let response = LatestConfigsResponse {
            config_metas: Some(ConfigMetas {
                roots: vec![TopMeta {
                    version: config_root_version,
                    raw: config_root.clone(),
                }],
                timestamp: Some(TopMeta {
                    version: config_timestamp_version,
                    raw: config_timestamp.clone(),
                }),
                snapshot: Some(TopMeta {
                    version: config_snapshot_version,
                    raw: config_snapshot.clone(),
                }),
                top_targets: Some(TopMeta {
                    version: config_targets_version,
                    raw: config_targets.clone(),
                }),
                delegated_targets: delegated,
            }),
            director_metas: Some(DirectorMetas {
                roots: vec![TopMeta {
                    version: director_root_version,
                    raw: director_root.clone(),
                }],
                timestamp: Some(TopMeta {
                    version: director_timestamp_version,
                    raw: director_timestamp.clone(),
                }),
                snapshot: Some(TopMeta {
                    version: director_snapshot_version,
                    raw: director_snapshot.clone(),
                }),
                targets: Some(TopMeta {
                    version: director_targets_version,
                    raw: director_targets.clone(),
                }),
            }),
            target_files: target_files
                .into_iter()
                .map(|(path, raw)| File { path, raw })
                .collect(),
        };

        self.uptane.update(&response)?;
        Ok(())
    }

    /// Loads raw target files from the Uptane cache for the selected configurations.
    fn load_target_files(&self, matched: &[String]) -> Result<HashMap<String, Vec<u8>>, CdnError> {
        let mut files = HashMap::new();
        for path in matched {
            if let Some(raw) = self.uptane.target_file(path)? {
                files.insert(path.clone(), raw);
            }
        }
        Ok(files)
    }

    /// Collects director roots newer than the `current_root_version` so lagging clients can catch up.
    fn collect_new_roots(
        &self,
        current_root_version: u64,
        versions: &TufVersions,
    ) -> Result<Vec<Vec<u8>>, CdnError> {
        if versions.director_root <= current_root_version {
            return Ok(Vec::new());
        }
        self.uptane
            .director_roots_since(current_root_version, versions.director_root)
            .map_err(CdnError::Uptane)
    }

    /// Fetches all delegated metadata referenced in the provided targets document.
    async fn fetch_delegated_metadata(
        &self,
        top_targets: &[u8],
    ) -> Result<Vec<DelegatedMeta>, CdnError> {
        let value: Value = serde_json::from_slice(top_targets)?;
        let roles = value
            .pointer("/signed/delegations/roles")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        let mut delegated = Vec::new();
        if let Value::Array(entries) = roles {
            for role in entries {
                let Some(name) = role.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let path = format!("config/delegated/{name}.json");
                let (raw, version) = self.fetch_top_meta(&path).await?;
                delegated.push(DelegatedMeta {
                    version,
                    role: name.to_string(),
                    raw,
                });
            }
        }
        Ok(delegated)
    }

    /// Fetches a metadata file from the CDN and returns its raw bytes plus the version number.
    /// Downloads one of the top-level metadata files (root/timestamp/snapshot/targets) and returns its bytes + version.
    async fn fetch_top_meta(&self, path: &str) -> Result<(Vec<u8>, u64), CdnError> {
        let bytes = self.fetch_bytes(path).await?;
        let version = extract_version(&bytes)?;
        Ok((bytes, version))
    }

    /// Downloads target files referenced in the provided director targets document.
    /// Downloads the target files listed in the provided serialized targets metadata.
    async fn fetch_target_files(&self, targets: &[u8]) -> Result<Vec<(String, Vec<u8>)>, CdnError> {
        let value: Value = serde_json::from_slice(targets)?;
        let Some(obj) = value.pointer("/signed/targets").and_then(Value::as_object) else {
            return Ok(Vec::new());
        };
        let mut files = Vec::new();
        for (path, _) in obj {
            let bytes = self.fetch_bytes(path).await?;
            files.push((path.clone(), bytes));
        }
        Ok(files)
    }

    /// Issues a GET request against the CDN and returns the raw response body.
    /// Performs a throttled HTTP GET against the CDN for the given path.
    async fn fetch_bytes(&self, path: &str) -> Result<Vec<u8>, CdnError> {
        let mut url = self.base_url.clone();
        let normalised = format!(
            "/{}/{}",
            self.path_prefix.trim_end_matches('/'),
            path.trim_start_matches('/')
        );
        url.set_path(&normalised);

        let namespace = CdnNamespace::from_path(path);
        let mut headers = HeaderMap::new();
        headers.insert(
            CDN_API_KEY_HEADER,
            HeaderValue::from_str(&self.api_key).map_err(|_| CdnError::Unauthorized)?,
        );

        let token = {
            let guard = self.state.lock().await;
            guard.auth_tokens.get(&namespace).cloned()
        };
        if let Some(token) = token {
            headers.insert(
                "Authorization",
                HeaderValue::from_str(&token).map_err(|_| CdnError::Unauthorized)?,
            );
        }

        let response = self.http.get(url).headers(headers).send().await?;
        match response.status() {
            StatusCode::OK => {
                if let Some(token) = response
                    .headers()
                    .get(CDN_REFRESHED_AUTH_HEADER)
                    .and_then(|value| value.to_str().ok())
                {
                    let mut guard = self.state.lock().await;
                    guard.auth_tokens.insert(namespace, token.to_string());
                }
                Ok(response.bytes().await?.to_vec())
            }
            StatusCode::UNAUTHORIZED => Err(CdnError::Unauthorized),
            StatusCode::NOT_FOUND => Err(CdnError::NotFound(path.to_string())),
            status => Err(CdnError::Status(status.as_u16())),
        }
    }

    #[cfg(test)]
    /// Test-only helper that exposes [`fetch_bytes`] directly.
    pub(crate) async fn fetch_bytes_for_test(&self, path: &str) -> Result<Vec<u8>, CdnError> {
        self.fetch_bytes(path).await
    }
}

/// Resolves the CDN base URL either from the provided site or the override.
fn resolve_cdn_base_url(config: &CdnClientConfig) -> Result<Url, CdnError> {
    if let Some(override_url) = &config.base_url_override {
        return Url::parse(override_url).map_err(|err| CdnError::InvalidBaseUrl(err.to_string()));
    }
    let host = resolve_cdn_host(&config.site);
    let full = format!("https://{host}");
    Url::parse(&full).map_err(|err| CdnError::InvalidBaseUrl(err.to_string()))
}

/// Computes the CDN hostname used for the provided site.
fn resolve_cdn_host(site: &str) -> String {
    let stripped = site.trim_start_matches("https://");
    match stripped {
        "datad0g.com" => "remote-config.datad0g.com".to_string(),
        "ap1.datadoghq.com" => "remote-config.datadoghq.com".to_string(),
        "us5.datadoghq.com" => "remote-config.datadoghq.com".to_string(),
        "us3.datadoghq.com" => "remote-config.datadoghq.com".to_string(),
        "app.datadoghq.eu" => "remote-config.datadoghq.com".to_string(),
        "app.datadoghq.com" => "remote-config.datadoghq.com".to_string(),
        "app.ddog-gov.com" => "remote-config.ddog-gov.com".to_string(),
        _ => "remote-config.datadoghq.com".to_string(),
    }
}

/// Computes the CDN path prefix derived from the site.
fn resolve_cdn_path_prefix(site: &str) -> String {
    let mut stripped = site.trim_start_matches("https://");
    stripped = stripped.trim_start_matches("app.");
    stripped.to_string()
}

/// Returns the bare domain used for employee bypass checks.
fn resolve_site_domain(site: &str) -> String {
    let stripped = site.trim_start_matches("https://");
    stripped.trim_start_matches("app.").to_string()
}

/// Extracts the metadata version from a JSON document.
fn extract_version(bytes: &[u8]) -> Result<u64, CdnError> {
    let value: Value = serde_json::from_slice(bytes)?;
    value
        .pointer("/signed/version")
        .and_then(Value::as_u64)
        .ok_or_else(|| CdnError::Validation("missing version field in metadata".to_string()))
}

/// Filters the director targets for the requested products.
fn select_targets_for_products(
    products: &[String],
    targets: &Value,
) -> Result<Vec<String>, CdnError> {
    let client = Client {
        products: products.to_vec(),
        ..Default::default()
    };
    compute_matched_configs(&client, targets.clone(), None)
        .map_err(|err| CdnError::Validation(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::RcStore;
    use hex::encode as hex_encode;
    use httptest::{matchers::*, responders::*, Expectation, Server};
    use remote_config_proto::remoteconfig::{
        ConfigMetas, DirectorMetas, File, LatestConfigsResponse, TopMeta,
    };
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    const CDN_TARGET_A_PATH: &str = "datadog/2/APM_TRACING/config/pkg1.json";
    const CDN_TARGET_B_PATH: &str = "datadog/2/ASM_DATA/config/pkg2.json";
    const CDN_TARGET_A_BYTES: &[u8] = br#"{"foo":"bar"}"#;
    const CDN_TARGET_B_BYTES: &[u8] = br#"{"baz":"qux"}"#;

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex_encode(hasher.finalize())
    }

    /// Builds deterministic metadata documents for tests.
    fn mk_meta(meta_type: &str, version: u64) -> Vec<u8> {
        serde_json::json!({
            "signed": {
                "_type": meta_type,
                "version": version,
                "expires": "2030-01-01T00:00:00Z"
            },
            "signatures": []
        })
        .to_string()
        .into_bytes()
    }

    /// Builds a targets document referencing two products.
    fn mk_targets(version: u64) -> Vec<u8> {
        serde_json::json!({
            "signed": {
                "_type": "targets",
                "version": version,
                "expires": "2030-01-01T00:00:00Z",
                "targets": {
                    CDN_TARGET_A_PATH: {
                        "length": CDN_TARGET_A_BYTES.len(),
                        "hashes": { "sha256": sha256_hex(CDN_TARGET_A_BYTES) },
                        "custom": { "expires": 0 }
                    },
                    CDN_TARGET_B_PATH: {
                        "length": CDN_TARGET_B_BYTES.len(),
                        "hashes": { "sha256": sha256_hex(CDN_TARGET_B_BYTES) },
                        "custom": { "expires": 0 }
                    }
                }
            },
            "signatures": []
        })
        .to_string()
        .into_bytes()
    }

    /// Builds a `TopMeta` payload for the provided metadata type/version.
    fn top_meta_record(meta_type: &str, version: u64) -> TopMeta {
        TopMeta {
            version,
            raw: mk_meta(meta_type, version),
        }
    }

    /// Builds a targets metadata record for the requested version.
    fn top_targets_meta(version: u64) -> TopMeta {
        TopMeta {
            version,
            raw: mk_targets(version),
        }
    }

    /// Synthesises a [`LatestConfigsResponse`] for Uptane fixture seeding.
    fn latest_configs_response(root_version: u64, targets_version: u64) -> LatestConfigsResponse {
        LatestConfigsResponse {
            config_metas: Some(ConfigMetas {
                roots: vec![top_meta_record("root", root_version)],
                timestamp: Some(top_meta_record("timestamp", root_version)),
                snapshot: Some(top_meta_record("snapshot", root_version)),
                top_targets: Some(top_targets_meta(root_version)),
                delegated_targets: vec![],
            }),
            director_metas: Some(DirectorMetas {
                roots: vec![top_meta_record("root", root_version)],
                timestamp: Some(top_meta_record("timestamp", root_version)),
                snapshot: Some(top_meta_record("snapshot", targets_version)),
                targets: Some(top_targets_meta(targets_version)),
            }),
            target_files: vec![
                File {
                    path: CDN_TARGET_A_PATH.to_string(),
                    raw: CDN_TARGET_A_BYTES.to_vec(),
                },
                File {
                    path: CDN_TARGET_B_PATH.to_string(),
                    raw: CDN_TARGET_B_BYTES.to_vec(),
                },
            ],
            ..Default::default()
        }
    }

    /// Installs an expectation for a GET call on the stub CDN server.
    fn expect_restricted_get(server: &Server, path: &str, body: Vec<u8>, times: usize) {
        server.expect(
            Expectation::matching(all_of![
                request::method("GET"),
                request::path(format!("/datadoghq.com/{path}"))
            ])
            .times(times)
            .respond_with(status_code(200).body(body)),
        );
    }

    /// Configures a stub CDN server returning canned responses.
    fn setup_stub_server() -> (Server, Vec<(String, Vec<u8>)>) {
        let server = Server::run();

        let metadata = vec![
            ("config/root.json", mk_meta("root", 1)),
            ("config/timestamp.json", mk_meta("timestamp", 1)),
            ("config/snapshot.json", mk_meta("snapshot", 1)),
            ("config/targets.json", mk_targets(1)),
            ("director/root.json", mk_meta("root", 1)),
            ("director/timestamp.json", mk_meta("timestamp", 1)),
            ("director/snapshot.json", mk_meta("snapshot", 1)),
            ("director/targets.json", mk_targets(1)),
        ];

        for (path, body) in &metadata {
            server.expect(
                Expectation::matching(all_of![
                    request::method("GET"),
                    request::path(format!("/datadoghq.com/{}", path))
                ])
                .respond_with(status_code(200).body(body.clone())),
            );
        }

        let targets = vec![
            (CDN_TARGET_A_PATH.to_string(), CDN_TARGET_A_BYTES.to_vec()),
            (CDN_TARGET_B_PATH.to_string(), CDN_TARGET_B_BYTES.to_vec()),
        ];

        for (path, body) in &targets {
            server.expect(
                Expectation::matching(all_of![
                    request::method("GET"),
                    request::path(format!("/datadoghq.com/{}", path))
                ])
                .respond_with(status_code(200).body(body.clone())),
            );
        }

        (server, targets)
    }

    #[tokio::test]
    /// Ensures the CDN client retrieves metadata and filters target files by product.
    async fn cdn_client_fetches_and_filters_targets() {
        let (server, targets) = setup_stub_server();
        let tmp = TempDir::new().unwrap();
        let config = CdnClientConfig {
            site: "https://app.datadoghq.com".to_string(),
            api_key: "test-key".to_string(),
            agent_version: "1.2.3".to_string(),
            throttle: None,
            base_url_override: Some(server.url_str("")),
            config_root_override: None,
            director_root_override: None,
        };

        let client = CdnClient::with_run_path(tmp.path(), config).unwrap();
        let products = vec!["APM_TRACING".to_string()];
        let update = client
            .get_update(&products, 0, 0)
            .await
            .unwrap()
            .expect("update expected");

        assert_eq!(
            update.client_configs,
            vec!["datadog/2/APM_TRACING/config/pkg1.json"]
        );
        assert_eq!(update.target_files.len(), 1);
        assert_eq!(
            update.target_files["datadog/2/APM_TRACING/config/pkg1.json"],
            targets[0].1
        );
        assert!(!update.tuf_targets.is_empty());
        assert!(!update.tuf_roots.is_empty());

        // Subsequent call with the same targets version returns no update.
        let none = client.get_update(&products, 1, 1).await.unwrap();
        assert!(none.is_none());
    }

    #[tokio::test]
    /// Ensures throttling prevents excessive refresh attempts within the window.
    async fn cdn_client_respects_throttle_window() {
        let server = Server::run();
        for (path, body) in [
            ("config/root.json", mk_meta("root", 1)),
            ("config/timestamp.json", mk_meta("timestamp", 1)),
            ("config/snapshot.json", mk_meta("snapshot", 1)),
            ("config/targets.json", mk_targets(1)),
            ("director/root.json", mk_meta("root", 1)),
            ("director/timestamp.json", mk_meta("timestamp", 1)),
            ("director/snapshot.json", mk_meta("snapshot", 1)),
            ("director/targets.json", mk_targets(1)),
        ] {
            expect_restricted_get(&server, path, body, 1);
        }
        for (path, body) in [
            (
                "datadog/2/APM_TRACING/config/pkg1.json",
                br#"{"foo":"bar"}"#.to_vec(),
            ),
            (
                "datadog/2/ASM_DATA/config/pkg2.json",
                br#"{"baz":"qux"}"#.to_vec(),
            ),
        ] {
            expect_restricted_get(&server, path, body, 1);
        }

        let tmp = TempDir::new().unwrap();
        let config = CdnClientConfig {
            site: "https://app.datadoghq.com".to_string(),
            api_key: "test-key".to_string(),
            agent_version: "1.2.3".to_string(),
            throttle: Some(Duration::from_secs(60)),
            base_url_override: Some(server.url_str("")),
            config_root_override: None,
            director_root_override: None,
        };
        let client = CdnClient::with_run_path(tmp.path(), config).unwrap();
        let products = vec!["APM_TRACING".to_string()];

        let update_one = client
            .get_update(&products, 0, 0)
            .await
            .unwrap()
            .expect("first update expected");
        assert!(!update_one.tuf_roots.is_empty());

        let update_two = client
            .get_update(&products, 0, 0)
            .await
            .unwrap()
            .expect("cached update expected");
        assert_eq!(update_two.client_configs, update_one.client_configs);
    }

    #[tokio::test]
    /// Ensures cached data is returned even when refresh fails.
    async fn cdn_client_returns_cached_on_refresh_failure() {
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method("GET"),
                request::path("/datadoghq.com/config/root.json")
            ])
            .respond_with(status_code(500)),
        );

        let tmp = TempDir::new().unwrap();
        let config = CdnClientConfig {
            site: "https://app.datadoghq.com".to_string(),
            api_key: "test-key".to_string(),
            agent_version: "1.2.3".to_string(),
            throttle: None,
            base_url_override: Some(server.url_str("")),
            config_root_override: None,
            director_root_override: None,
        };
        let client = CdnClient::with_run_path(tmp.path(), config).unwrap();
        let latest = latest_configs_response(1, 1);
        client.uptane_state().update(&latest).expect("seed store");

        let products = vec!["APM_TRACING".to_string()];
        let update = client
            .get_update(&products, 0, 0)
            .await
            .unwrap()
            .expect("cached update expected");
        let expected_path = latest.target_files[0].path.clone();
        assert_eq!(update.client_configs, vec![expected_path.clone()]);
        assert_eq!(update.target_files.len(), 1);
        assert_eq!(
            update.target_files[&expected_path],
            latest.target_files[0].raw
        );
        assert!(!update.tuf_roots.is_empty());
    }

    #[tokio::test]
    /// Ensures stale caches trigger a refresh even when a previous update succeeded.
    async fn cdn_client_refreshes_after_stale_window() {
        let server = Server::run();
        for (path, body) in [
            ("config/root.json", mk_meta("root", 1)),
            ("config/timestamp.json", mk_meta("timestamp", 1)),
            ("config/snapshot.json", mk_meta("snapshot", 1)),
            ("config/targets.json", mk_targets(1)),
            ("director/root.json", mk_meta("root", 1)),
            ("director/timestamp.json", mk_meta("timestamp", 1)),
            ("director/snapshot.json", mk_meta("snapshot", 1)),
            ("director/targets.json", mk_targets(1)),
        ] {
            expect_restricted_get(&server, path, body, 2);
        }
        for (path, body) in [
            (
                "datadog/2/APM_TRACING/config/pkg1.json",
                br#"{"foo":"bar"}"#.to_vec(),
            ),
            (
                "datadog/2/ASM_DATA/config/pkg2.json",
                br#"{"baz":"qux"}"#.to_vec(),
            ),
        ] {
            expect_restricted_get(&server, path, body, 2);
        }

        let tmp = TempDir::new().unwrap();
        let config = CdnClientConfig {
            site: "https://app.datadoghq.com".to_string(),
            api_key: "test-key".to_string(),
            agent_version: "1.2.3".to_string(),
            throttle: Some(Duration::from_secs(1)),
            base_url_override: Some(server.url_str("")),
            config_root_override: None,
            director_root_override: None,
        };

        let client = CdnClient::with_run_path(tmp.path(), config).unwrap();
        let products = vec!["APM_TRACING".to_string()];

        let update_one = client
            .get_update(&products, 0, 0)
            .await
            .unwrap()
            .expect("initial update expected");
        assert!(!update_one.tuf_roots.is_empty());

        {
            let mut guard = client.state.lock().await;
            guard.last_update = Some(Instant::now() - Duration::from_secs(5));
        }

        let update_two = client
            .get_update(&products, 0, 0)
            .await
            .unwrap()
            .expect("stale cache should trigger refresh");
        assert!(!update_two.tuf_roots.is_empty());
    }

    #[tokio::test]
    /// Ensures the CDN client scopes refreshed auth tokens per repository namespace.
    async fn cdn_client_scopes_tokens_per_namespace() {
        let server = Server::run();
        let config_token = "Bearer config-token";
        let director_token = "Bearer director-token";

        server.expect(
            Expectation::matching(all_of![
                request::method("GET"),
                request::path("/datadoghq.com/config/root.json"),
                request::headers(not(contains(key("authorization"))))
            ])
            .respond_with(
                status_code(200)
                    .append_header(CDN_REFRESHED_AUTH_HEADER, config_token)
                    .body(mk_meta("root", 1)),
            ),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method("GET"),
                request::path("/datadoghq.com/config/timestamp.json"),
                request::headers(contains(("authorization", config_token)))
            ])
            .respond_with(status_code(200).body(mk_meta("timestamp", 1))),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method("GET"),
                request::path("/datadoghq.com/director/root.json"),
                request::headers(not(contains(key("authorization"))))
            ])
            .respond_with(
                status_code(200)
                    .append_header(CDN_REFRESHED_AUTH_HEADER, director_token)
                    .body(mk_meta("root", 1)),
            ),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method("GET"),
                request::path("/datadoghq.com/director/timestamp.json"),
                request::headers(contains(("authorization", director_token)))
            ])
            .respond_with(status_code(200).body(mk_meta("timestamp", 1))),
        );

        let tmp = TempDir::new().unwrap();
        let config = CdnClientConfig {
            site: "https://app.datadoghq.com".to_string(),
            api_key: "test-key".to_string(),
            agent_version: "1.2.3".to_string(),
            throttle: None,
            base_url_override: Some(server.url_str("")),
            config_root_override: None,
            director_root_override: None,
        };

        let client = CdnClient::with_run_path(tmp.path(), config).unwrap();
        client
            .fetch_bytes_for_test("config/root.json")
            .await
            .unwrap();
        client
            .fetch_bytes_for_test("config/timestamp.json")
            .await
            .unwrap();
        client
            .fetch_bytes_for_test("director/root.json")
            .await
            .unwrap();
        client
            .fetch_bytes_for_test("director/timestamp.json")
            .await
            .unwrap();
    }

    #[tokio::test]
    /// Ensures Uptane exposes the complete director root chain when cached.
    async fn cdn_client_returns_complete_root_chain() {
        let store =
            RcStore::open_ephemeral("1.2.3", "test-key", "https://config.datadoghq.com").unwrap();
        let config = CdnClientConfig {
            site: "https://app.datadoghq.com".to_string(),
            api_key: "test-key".to_string(),
            agent_version: "1.2.3".to_string(),
            throttle: None,
            base_url_override: None,
            config_root_override: None,
            director_root_override: None,
        };
        let base_url = Url::parse("https://remote-config.datadoghq.com").unwrap();
        let client =
            CdnClient::with_store(store, config, base_url, "datadoghq.com".to_string()).unwrap();

        let uptane = client.uptane_state();
        uptane
            .update(&latest_configs_response(1, 1))
            .expect("initial update should succeed");
        uptane
            .update(&latest_configs_response(2, 2))
            .expect("follow-up update should succeed");

        {
            let mut guard = client.state.lock().await;
            guard.last_update = Some(Instant::now());
        }

        let products = vec!["APM_TRACING".to_string()];
        let update = client
            .get_update(&products, 0, 0)
            .await
            .unwrap()
            .expect("expected CDN update");

        let versions: Vec<u64> = update
            .tuf_roots
            .iter()
            .map(|bytes| {
                serde_json::from_slice::<serde_json::Value>(bytes)
                    .unwrap()
                    .pointer("/signed/version")
                    .and_then(|v| v.as_u64())
                    .unwrap()
            })
            .collect();
        assert_eq!(versions, vec![1, 2]);
    }
}
