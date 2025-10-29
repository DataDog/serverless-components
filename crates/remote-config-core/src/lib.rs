//! Public entry points for the remote-config core crate.
//!
//! The module re-exports the building blocks required to bootstrap the
//! service, interact with CDN helpers, and embed Remote Config in host
//! applications without digging into the internal module layout.

pub mod bootstrap;
pub mod cdn;
pub mod config;
mod embedded_roots;
pub mod http;
pub mod rc_key;
pub mod service;
pub mod status;
pub mod store;
pub mod targets;
pub mod telemetry;
pub mod uptane;
pub mod uptane_path;

pub use bootstrap::{
    handle_client_request, handle_client_request_bytes, BootstrapArtifacts, BootstrapError,
    CredentialSnapshot, CredentialWatch, HostRuntimeSnapshot, RemoteConfigBootstrap,
    RemoteConfigHost, RemoteConfigRuntime, RuntimeWatch, StoreMode, WatchedRemoteConfigRuntime,
};
pub use cdn::{CdnClient, CdnClientConfig, CdnError, CdnUpdate};
pub use config::{sanitize_api_key, RemoteConfigEnv};
pub use embedded_roots::{resolve as resolve_embedded_roots, EmbeddedRootError, RootBundle};
pub use rc_key::{RcKey, RcKeyError};
pub use service::{
    CacheBypassSignal, ClientSnapshot, OrgStatusSnapshot, RefreshOutcome, RemoteConfigHandle,
    RemoteConfigService, RemoteConfigTelemetry, RuntimeStateSnapshot, ServiceConfig, ServiceError,
    ServiceSnapshot, WebsocketCheckResult,
};
pub use status::{status_json, RemoteConfigStatus, StatusSnapshot};
pub use store::{Metadata, RcStore, RcTree, StoreError};
pub use telemetry::{CompositeTelemetry, CountingTelemetry, TelemetryCounters, TelemetrySnapshot};
pub use uptane::{
    MetaState, OrgBindingConfig, SnapshotOrgBinding, State, TufVersions, UptaneConfig, UptaneError,
    UptaneState,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensures callers can construct a `RemoteConfigBootstrap` through the crate root.
    #[test]
    fn bootstrap_types_are_reexported() {
        let mut env = RemoteConfigEnv::from_env_iter::<Vec<(String, String)>, _, _>(Vec::new());
        env.api_key = Some("test-key".into());
        let bootstrap = RemoteConfigBootstrap {
            env,
            agent_version: "1.0.0",
            agent_uuid: Some("agent-uuid"),
            tags: vec!["env:test".into()],
            store: StoreMode::Ephemeral,
            runtime: None,
        };
        assert_eq!(bootstrap.agent_version, "1.0.0");
    }

    /// Verifies the status helpers exported at the crate root remain usable.
    #[tokio::test]
    async fn status_helpers_work_via_reexports() {
        let status = RemoteConfigStatus::new();
        status.set_org_flags(true, false);
        let json = status_json(&status).await;
        assert_eq!(json.get("orgEnabled").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            json.get("apiKeyScoped").and_then(|v| v.as_bool()),
            Some(false)
        );
    }
}
