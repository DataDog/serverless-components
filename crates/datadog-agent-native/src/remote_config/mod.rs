//! Remote Configuration integration for the Datadog Agent.
//!
//! This module provides a wrapper around the `remote-config-core` crate, making
//! Remote Config functionality available to the agent. Remote Config allows the agent
//! to receive configuration updates dynamically from the Datadog backend without
//! requiring restarts.
//!
//! ## Features
//!
//! - **Configuration polling**: Periodically fetches configuration from the backend
//! - **Uptane/TUF security**: Validates configuration integrity and authenticity
//! - **Persistent cache**: Stores configuration locally using sled
//! - **CDN support**: Can fetch configuration from CDN endpoints
//! - **Websocket diagnostics**: Supports websocket-based configuration delivery
//!
//! ## Configuration
//!
//! Remote Config is configured via environment variables:
//!
//! - `DD_REMOTE_CONFIGURATION_ENABLED` (default: `true`): Enable/disable Remote Config
//! - `DD_REMOTE_CONFIGURATION_API_KEY` or `DD_API_KEY`: API key for authentication
//! - `DD_REMOTE_CONFIGURATION_KEY`: Legacy RC key (optional)
//! - `DD_SITE` (default: `datadoghq.com`): Datadog site
//! - `DD_REMOTE_CONFIGURATION_NO_TLS` (default: `false`): Disable TLS
//! - `DD_REMOTE_CONFIGURATION_NO_TLS_VALIDATION` (default: `false`): Skip TLS validation
//!
//! ## Usage
//!
//! The simplest way to use Remote Config is via the bootstrap API:
//!
//! ```rust,no_run
//! use datadog_agent_native::remote_config;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Bootstrap from environment variables
//! let rc_env = remote_config::RemoteConfigEnv::from_os_env();
//!
//! if !rc_env.enabled {
//!     println!("Remote Config is disabled");
//!     return Ok(());
//! }
//!
//! // Create the service (requires API key and store path)
//! // See remote-config-core documentation for full setup
//! # Ok(())
//! # }
//! ```

// Re-export the main types from remote-config-core
pub use remote_config_core::{
    // Bootstrap utilities
    handle_client_request,
    handle_client_request_bytes,
    CredentialSnapshot,
    CredentialWatch,
    HostRuntimeSnapshot,
    // Configuration
    sanitize_api_key,
    BootstrapArtifacts,
    BootstrapError,
    RemoteConfigRuntime,
    // Service types
    CacheBypassSignal,
    // CDN client
    CdnClient,
    CdnClientConfig,
    CdnError,
    CdnUpdate,
    ClientSnapshot,
    // Telemetry
    CompositeTelemetry,
    CountingTelemetry,
    // Uptane
    MetaState,
    // Store
    Metadata,
    OrgStatusSnapshot,
    RcStore,
    RcTree,
    RefreshOutcome,
    RemoteConfigBootstrap,
    RemoteConfigEnv,
    RemoteConfigHandle,
    RemoteConfigHost,
    RemoteConfigService,
    RemoteConfigTelemetry,
    RuntimeWatch,
    ServiceConfig,
    ServiceError,
    ServiceSnapshot,
    State,
    StoreError,
    StoreMode,
    TelemetryCounters,
    TelemetrySnapshot,
    TufVersions,
    UptaneError,
    UptaneState,
    WebsocketCheckResult,
    WatchedRemoteConfigRuntime,
};

// Re-export proto types for convenience
pub use remote_config_proto;
