//! Shared fixtures and utilities for remote-config service tests.
//!
//! Consolidating these helpers keeps individual test modules focused on their
//! assertions while avoiding duplication of setup logic.

#![cfg(test)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use httptest::Server;
use remote_config_proto::remoteconfig::{
    Client, ClientState, ClientTracer, ConfigMetas, DirectorMetas, File, LatestConfigsResponse,
    TopMeta,
};
use serde_json::{json, Value};
use tempfile::TempDir;

use super::telemetry::RemoteConfigTelemetry;
use super::RemoteConfigService;
use super::{ServiceConfig, ServiceError};
use crate::embedded_roots;
use crate::http::{Auth, BackoffConfig, HttpClient, HttpClientOptions};
use crate::store::RcStore;
use crate::uptane::{UptaneConfig, UptaneState};

/// Canonical target path mirroring backend-issued configuration paths.
pub(crate) const SAMPLE_TARGET_PATH: &str = "datadog/12345/APM_TRACING/config/pkg1.json";
/// Secondary target path leveraged for predicate filtering tests.
pub(crate) const SAMPLE_SECOND_TARGET_PATH: &str = "datadog/12345/APM_TRACING/config/pkg2.json";
/// Canonical sample payload served in test responses.
pub(crate) const SAMPLE_TARGET_PAYLOAD: &[u8; 7] = b"payload";
/// SHA-256 hash of [`SAMPLE_TARGET_PAYLOAD`].
pub(crate) const SAMPLE_TARGET_HASH: &str =
    "239f59ed55e737c77147cf55ad0c1b030b6d7ee748a7426952f9b852d5a935e5";

/// Creates a fully populated response mirroring the backend payload shape.
pub(crate) fn sample_response(config_version: u64, director_version: u64) -> LatestConfigsResponse {
    LatestConfigsResponse {
        config_metas: Some(ConfigMetas {
            roots: mk_root_chain(config_version),
            timestamp: Some(mk_timestamp_meta(config_version, "2030-01-01T00:00:00Z")),
            snapshot: Some(mk_top_meta("snapshot", config_version)),
            top_targets: Some(mk_targets_meta(config_version)),
            delegated_targets: vec![],
        }),
        director_metas: Some(DirectorMetas {
            roots: mk_root_chain(director_version),
            timestamp: Some(mk_timestamp_meta(director_version, "2030-01-01T00:00:00Z")),
            snapshot: Some(mk_top_meta("snapshot", director_version)),
            targets: Some(mk_targets_meta(director_version)),
        }),
        target_files: vec![File {
            path: SAMPLE_TARGET_PATH.to_string(),
            raw: SAMPLE_TARGET_PAYLOAD.to_vec(),
        }],
        ..Default::default()
    }
}

/// Provides a baseline response without refresh overrides for comparison tests.
pub(crate) fn response_without_override() -> LatestConfigsResponse {
    LatestConfigsResponse {
        config_metas: Some(ConfigMetas {
            roots: mk_root_chain(1),
            timestamp: Some(mk_timestamp_meta(1, "2030-01-01T00:00:00Z")),
            snapshot: Some(mk_top_meta("snapshot", 1)),
            top_targets: Some(mk_targets_meta_without_override(1)),
            delegated_targets: vec![],
        }),
        director_metas: Some(DirectorMetas {
            roots: mk_root_chain(1),
            timestamp: Some(mk_timestamp_meta(1, "2030-01-01T00:00:00Z")),
            snapshot: Some(mk_top_meta("snapshot", 1)),
            targets: Some(mk_targets_meta_without_override(1)),
        }),
        ..Default::default()
    }
}

/// Builds a top-level metadata blob containing minimal fields.
pub(crate) fn mk_top_meta(meta_type: &str, version: u64) -> TopMeta {
    let raw = json!({
        "signed": {
            "_type": meta_type,
            "version": version,
            "expires": "2030-01-01T00:00:00Z"
        },
        "signatures": []
    })
    .to_string()
    .into_bytes();
    TopMeta { version, raw }
}

/// Builds a snapshot metadata document with an optional `org_uuid` custom field.
pub(crate) fn mk_snapshot_meta_with_org(version: u64, org_uuid: Option<&str>) -> TopMeta {
    let mut signed = json!({
        "_type": "snapshot",
        "version": version,
        "expires": "2030-01-01T00:00:00Z",
        "meta": {
            "targets.json": { "version": version }
        }
    });
    if let Some(uuid) = org_uuid {
        // Attach the org binding so tests can exercise snapshot verification.
        signed["custom"] = json!({ "org_uuid": uuid });
    }
    let raw = json!({
        "signed": signed,
        "signatures": []
    })
    .to_string()
    .into_bytes();
    TopMeta { version, raw }
}

/// Builds a contiguous root chain from version 1 up to the requested version.
pub(crate) fn mk_root_chain(version: u64) -> Vec<TopMeta> {
    (1..=version).map(|v| mk_top_meta("root", v)).collect()
}

/// Builds a timestamp metadata record using the provided expiry string.
pub(crate) fn mk_timestamp_meta(version: u64, expires: &str) -> TopMeta {
    let raw = json!({
        "signed": {
            "_type": "timestamp",
            "version": version,
            "expires": expires
        },
        "signatures": []
    })
    .to_string()
    .into_bytes();
    TopMeta { version, raw }
}

/// Creates a targets metadata document storing a single package entry with refresh overrides.
pub(crate) fn mk_targets_meta(version: u64) -> TopMeta {
    mk_targets_meta_from_value(
        version,
        json!({
            (SAMPLE_TARGET_PATH): {
                "length": SAMPLE_TARGET_PAYLOAD.len(),
                "hashes": { "sha256": SAMPLE_TARGET_HASH },
                "custom": {
                    "agent_refresh_interval": 5
                }
            }
        }),
        Some(json!({ "agent_refresh_interval": 5 })),
    )
}

/// Creates a targets metadata document without refresh overrides.
pub(crate) fn mk_targets_meta_without_override(version: u64) -> TopMeta {
    mk_targets_meta_from_value(
        version,
        json!({
            (SAMPLE_TARGET_PATH): {
                "length": SAMPLE_TARGET_PAYLOAD.len(),
                "hashes": { "sha256": SAMPLE_TARGET_HASH }
            }
        }),
        None,
    )
}

/// Helper creating a `TopMeta` document from a prepared targets object.
pub(crate) fn mk_targets_meta_from_value(
    version: u64,
    targets: Value,
    custom: Option<Value>,
) -> TopMeta {
    let mut signed = json!({
        "_type": "targets",
        "version": version,
        "expires": "2030-01-01T00:00:00Z",
        "targets": targets
    });
    if let Some(custom_value) = custom {
        signed["custom"] = custom_value;
    }
    let raw = json!({
        "signed": signed,
        "signatures": []
    })
    .to_string()
    .into_bytes();
    TopMeta { version, raw }
}

/// Returns the base configuration shared by the tests.
pub(crate) fn base_config() -> ServiceConfig {
    ServiceConfig {
        hostname: "test-host".into(),
        agent_version: "1.0.0".into(),
        agent_uuid: "uuid".into(),
        tags: vec!["env:test".into()],
        trace_agent_env: Some("test".into()),
        site: "datadoghq.com".into(),
        config_root_override: None,
        director_root_override: None,
        rc_key: None,
        default_refresh_interval: Duration::from_millis(200),
        min_refresh_interval: Duration::from_millis(50),
        org_status_interval: Duration::from_secs(0),
        cache_bypass_limit: 2,
        clients_ttl: Duration::from_secs(30),
        bypass_block_timeout: Duration::from_secs(2),
        new_client_block_timeout: Duration::from_secs(2),
        enable_websocket_echo: false,
        websocket_echo_interval: Duration::from_secs(60),
        websocket_echo_timeout: Duration::from_secs(2),
        disable_background_poller: true,
        allow_refresh_override: true,
        backoff_config: BackoffConfig::default(),
        enforce_go_limits: false,
    }
}

/// Builds a service instance using the provided configuration.
pub(crate) fn build_service_with_config(
    server: &Server,
    config: ServiceConfig,
) -> RemoteConfigService {
    let base_url = server.url_str("").trim_end_matches('/').to_string();
    build_service_with_base_url(base_url, config)
}

/// Builds a service connected to the supplied Remote Config backend URL.
pub(crate) fn build_service_with_base_url(
    base_url: String,
    config: ServiceConfig,
) -> RemoteConfigService {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.keep().join("rc.db");
    let store = RcStore::open(
        &db_path,
        "7.60.0",
        "test_api_key",
        "https://config.datadoghq.com",
    )
    .unwrap();
    let mut uptane_cfg = UptaneConfig::default();
    uptane_cfg.org_binding.expected_org_id = config.rc_key.as_ref().map(|key| key.org_id);
    let uptane = UptaneState::with_config(store, uptane_cfg).unwrap();
    let (config_root, director_root) = embedded_roots::resolve(
        &config.site,
        config.config_root_override.as_deref(),
        config.director_root_override.as_deref(),
    )
    .expect("embedded roots available");
    uptane
        .seed_trust_roots(&config_root.raw, &director_root.raw)
        .expect("seed trust anchors");
    uptane
        .update_org_uuid("00000000-0000-0000-0000-000000000000")
        .unwrap();
    let auth = Auth {
        api_key: "key".into(),
        application_key: None,
        par_jwt: None,
    };
    let http = HttpClient::new(
        base_url,
        &auth,
        "7.70.0",
        HttpClientOptions {
            allow_plaintext: true,
            accept_invalid_certs: true,
        },
    )
    .unwrap();
    RemoteConfigService::new(http, uptane, config)
}

/// Builds a service instance suitable for unit testing using default settings.
pub(crate) fn build_service(server: &Server) -> RemoteConfigService {
    build_service_with_config(server, base_config())
}

/// Produces a minimal tracer client descriptor accepted by the validator.
pub(crate) fn tracer_client(id: &str) -> Client {
    Client {
        id: id.to_string(),
        state: Some(ClientState {
            root_version: 1,
            ..Default::default()
        }),
        products: vec!["APM_TRACING".into()],
        is_tracer: true,
        client_tracer: Some(ClientTracer {
            runtime_id: format!("{id}-runtime"),
            language: "rust".into(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Registers an artificial client entry so calls are treated as existing.
pub(crate) async fn register_client_for_test(
    service: &RemoteConfigService,
    client_id: &str,
    products: &[&str],
) {
    let mut guard = service.shared_for_tests().state.lock().await;
    let products: Vec<String> = products.iter().map(|p| (*p).to_string()).collect();
    let descriptor = Client {
        id: client_id.to_string(),
        products: products.clone(),
        ..Default::default()
    };
    let ttl = service.config().clients_ttl;
    let _ = guard.register_client(&descriptor, ttl);
    for product in products {
        guard.products.insert(product.clone());
        guard.new_products.remove(&product);
    }
}

/// Telemetry implementation that records event counters for assertions.
#[derive(Default)]
pub(crate) struct RecordingTelemetry {
    /// Number of successful refreshes observed.
    pub success: AtomicUsize,
    /// Number of refresh errors observed.
    pub error: AtomicUsize,
    /// Number of bypass rejections observed.
    pub bypass_rejected: AtomicUsize,
    /// Number of rate-limited bypass requests observed.
    pub bypass_rate_limited: AtomicUsize,
    /// Number of websocket checks captured.
    pub websocket_checks: AtomicUsize,
}

impl RemoteConfigTelemetry for RecordingTelemetry {
    /// Records a successful refresh for later assertions.
    fn on_refresh_success(&self, _next_interval: Duration) {
        self.success.fetch_add(1, Ordering::Relaxed);
    }

    /// Increments the refresh error counter.
    fn on_refresh_error(&self, _error: &ServiceError) {
        self.error.fetch_add(1, Ordering::Relaxed);
    }

    /// Tracks rejected bypass requests.
    fn on_bypass_rejected(&self) {
        self.bypass_rejected.fetch_add(1, Ordering::Relaxed);
    }

    /// Tracks rate-limited bypass attempts.
    fn on_bypass_rate_limited(&self) {
        self.bypass_rate_limited.fetch_add(1, Ordering::Relaxed);
    }

    /// Captures websocket check invocations.
    fn on_websocket_check(&self, _result: super::state::WebsocketCheckResult) {
        self.websocket_checks.fetch_add(1, Ordering::Relaxed);
    }
}
