//! Utilities for constructing protobuf payloads used by integration tests.
use remote_config_proto::remoteconfig::{
    Client, ClientState, ClientTracer, ConfigMetas, DirectorMetas, File, LatestConfigsResponse,
    OrgDataResponse, OrgStatusResponse, TargetFileHash, TargetFileMeta,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::tuf::{build_signed_repo, TargetData};

/// Canonical configuration target path exercised by the scenarios.
pub const SAMPLE_TARGET_PATH: &str = "datadog/12345/APM_TRACING/config/sample.json";

/// Secondary target path leveraged to test predicate filtering.
pub const SAMPLE_SECOND_TARGET_PATH: &str = "datadog/12345/APM_TRACING/config/secondary.json";

/// Builds a minimal tracer client descriptor accepted by the service validator.
pub fn tracer_client(id: &str) -> Client {
    Client {
        id: id.to_string(),
        state: Some(ClientState {
            root_version: 1,
            ..Default::default()
        }),
        products: vec!["APM_TRACING".to_string()],
        is_tracer: true,
        client_tracer: Some(ClientTracer {
            runtime_id: format!("{id}-runtime"),
            language: "rust".to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Describes an individual target file that should appear in the response.
#[derive(Clone)]
pub struct TargetFixture {
    /// Remote Config storage path for the target.
    pub path: &'static str,
    /// Raw payload delivered to clients.
    pub payload: Vec<u8>,
    /// Optional metadata injected into the TUF targets document under `custom`.
    pub custom: Option<Value>,
}

/// Builder composing a full [`LatestConfigsResponse`] document with configurable metadata.
pub struct ResponseBuilder {
    config_version: u64,
    director_version: u64,
    config_timestamp: String,
    director_timestamp: String,
    refresh_override: Option<i64>,
    targets: Vec<TargetFixture>,
}

impl ResponseBuilder {
    /// Creates a builder initialised with matching configuration and director versions.
    pub fn new(config_version: u64, director_version: u64) -> Self {
        Self {
            config_version,
            director_version,
            config_timestamp: "2030-01-01T00:00:00Z".to_string(),
            director_timestamp: "2030-01-01T00:00:00Z".to_string(),
            refresh_override: None,
            targets: vec![TargetFixture {
                path: SAMPLE_TARGET_PATH,
                payload: br#"{ "default": true }"#.to_vec(),
                custom: None,
            }],
        }
    }

    /// Overrides the expiration timestamp applied to the configuration metadata.
    pub fn with_config_timestamp(mut self, value: &str) -> Self {
        self.config_timestamp = value.to_string();
        self
    }

    /// Overrides the expiration timestamp applied to the director metadata.
    pub fn with_director_timestamp(mut self, value: &str) -> Self {
        self.director_timestamp = value.to_string();
        self
    }

    /// Injects a custom refresh interval override (in seconds) into the top-level targets document.
    pub fn with_refresh_override(mut self, seconds: i64) -> Self {
        self.refresh_override = Some(seconds);
        self
    }

    /// Replaces the target list with the supplied fixtures.
    pub fn with_targets(mut self, fixtures: Vec<TargetFixture>) -> Self {
        self.targets = fixtures;
        self
    }

    /// Finalises the builder into a [`LatestConfigsResponse`].
    pub fn build(self) -> LatestConfigsResponse {
        let target_data: Vec<TargetData> = self
            .targets
            .iter()
            .map(|fixture| TargetData {
                path: fixture.path,
                payload: &fixture.payload,
                custom: fixture.custom.clone(),
            })
            .collect();
        let (config_roots, config_timestamp, config_snapshot, config_targets) = build_signed_repo(
            self.config_version,
            &self.config_timestamp,
            &target_data,
            self.refresh_override,
        );
        let (director_roots, director_timestamp, director_snapshot, director_targets) =
            build_signed_repo(
                self.director_version,
                &self.director_timestamp,
                &target_data,
                None,
            );

        LatestConfigsResponse {
            config_metas: Some(ConfigMetas {
                roots: config_roots,
                timestamp: Some(config_timestamp),
                snapshot: Some(config_snapshot),
                top_targets: Some(config_targets),
                delegated_targets: vec![],
            }),
            director_metas: Some(DirectorMetas {
                roots: director_roots,
                timestamp: Some(director_timestamp),
                snapshot: Some(director_snapshot),
                targets: Some(director_targets),
            }),
            target_files: self
                .targets
                .into_iter()
                .map(|fixture| File {
                    path: fixture.path.to_string(),
                    raw: fixture.payload,
                })
                .collect(),
            ..Default::default()
        }
    }
}

/// Produces an organisation data response containing a UUID.
pub fn org_data_response(uuid: &str) -> OrgDataResponse {
    OrgDataResponse {
        uuid: uuid.to_string(),
        ..Default::default()
    }
}

/// Produces an organisation status response reflecting enablement/authorisation state.
pub fn org_status_response(enabled: bool, authorized: bool) -> OrgStatusResponse {
    OrgStatusResponse {
        enabled,
        authorized,
    }
}

/// Builds a [`TargetFileMeta`] entry matching the supplied fixture, aiding cache validation.
pub fn target_meta_from_fixture(fixture: &TargetFixture) -> TargetFileMeta {
    TargetFileMeta {
        path: fixture.path.to_string(),
        length: fixture.payload.len() as i64,
        hashes: vec![TargetFileHash {
            algorithm: "sha256".to_string(),
            hash: compute_sha256(&fixture.payload),
        }],
    }
}

/// Computes the SHA-256 digest of the supplied payload encoded as a lowercase hex string.
fn compute_sha256(payload: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(payload);
    let digest = hasher.finalize();
    hex::encode(digest)
}
