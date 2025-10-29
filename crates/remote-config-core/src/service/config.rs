//! Static configuration for the remote-config service runtime.
//!
//! These settings describe how the service interacts with the backend and its
//! background workers. The defaults mirror the Go agent so the Rust
//! implementation can serve as a drop-in replacement.

use std::time::Duration;

use crate::http::BackoffConfig;
use tracing::warn;

/// Minimum refresh interval accepted by the service (mirrors Go's `minimalRefreshInterval`).
pub const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
/// Default refresh cadence when no backend override is present (matches Go's `defaultRefreshInterval`).
pub const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
/// Default organisation status polling interval.
pub const DEFAULT_ORG_STATUS_INTERVAL: Duration = Duration::from_secs(60);
/// Default cache-bypass allowance per refresh window.
pub const DEFAULT_CACHE_BYPASS_LIMIT: usize = 5;
/// Minimum cache-bypass allowance.
pub const MIN_CACHE_BYPASS_LIMIT: usize = 1;
/// Maximum cache-bypass allowance.
pub const MAX_CACHE_BYPASS_LIMIT: usize = 10;
/// Default TTL applied to client descriptors.
pub const DEFAULT_CLIENTS_TTL: Duration = Duration::from_secs(30);
/// Maximum TTL permitted for client descriptors (mirrors Go's `maxClientsTTL`).
pub const MAX_CLIENTS_TTL: Duration = Duration::from_secs(60);
/// Minimum backoff window supported by the backend option hooks.
pub const MIN_BACKOFF_INTERVAL: Duration = Duration::from_secs(2 * 60);
/// Maximum backoff window supported by the backend option hooks.
pub const MAX_BACKOFF_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Configuration values that control the remote configuration service runtime.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// Hostname reported to the remote configuration backend.
    pub hostname: String,
    /// Agent version string included in refresh requests.
    pub agent_version: String,
    /// Agent UUID identifying this instance towards the backend.
    pub agent_uuid: String,
    /// Tags propagated in refresh requests (e.g. `env:prod`).
    pub tags: Vec<String>,
    /// Optional environment string mirrored from the trace agent configuration.
    pub trace_agent_env: Option<String>,
    /// Datadog site (e.g. `datadoghq.com`) used to resolve embedded trust anchors.
    pub site: String,
    /// Optional override for the embedded config root metadata.
    pub config_root_override: Option<String>,
    /// Optional override for the embedded director root metadata.
    pub director_root_override: Option<String>,
    /// Optional decoded RC key used for org verification.
    pub rc_key: Option<crate::rc_key::RcKey>,
    /// Baseline polling cadence used after successful refreshes.
    pub default_refresh_interval: Duration,
    /// Lower bound for refresh delays, irrespective of backoff outcomes.
    pub min_refresh_interval: Duration,
    /// Cadence used to poll the organisation status endpoint.
    pub org_status_interval: Duration,
    /// Maximum number of cache-bypass refreshes allowed per refresh window.
    pub cache_bypass_limit: usize,
    /// Time-to-live applied to client activity records.
    pub clients_ttl: Duration,
    /// Maximum duration to wait for a bypass-triggered refresh acknowledgement.
    pub bypass_block_timeout: Duration,
    /// Maximum time new clients wait to enqueue a bypass request (mirrors Go's `newClientBlockTTL`).
    pub new_client_block_timeout: Duration,
    /// Whether the websocket echo tester should run.
    pub enable_websocket_echo: bool,
    /// Interval between websocket echo checks.
    pub websocket_echo_interval: Duration,
    /// Timeout applied to websocket echo exchanges.
    pub websocket_echo_timeout: Duration,
    /// When true the background refresh loop is disabled (manual refresh only).
    pub disable_background_poller: bool,
    /// Whether backend-provided refresh overrides are honoured.
    pub allow_refresh_override: bool,
    /// Backoff configuration applied when refreshes fail consecutively.
    pub backoff_config: BackoffConfig,
    /// When false the configuration skips Go-aligned safety clamps (intended for tests only).
    pub enforce_go_limits: bool,
}

impl Default for ServiceConfig {
    /// Returns the Go-aligned default configuration values for the service runtime.
    fn default() -> Self {
        Self {
            hostname: String::new(),
            agent_version: String::new(),
            agent_uuid: String::new(),
            tags: Vec::new(),
            trace_agent_env: None,
            site: "datadoghq.com".into(),
            config_root_override: None,
            director_root_override: None,
            rc_key: None,
            default_refresh_interval: DEFAULT_REFRESH_INTERVAL,
            min_refresh_interval: MIN_REFRESH_INTERVAL,
            org_status_interval: DEFAULT_ORG_STATUS_INTERVAL,
            cache_bypass_limit: DEFAULT_CACHE_BYPASS_LIMIT,
            clients_ttl: DEFAULT_CLIENTS_TTL,
            bypass_block_timeout: Duration::from_secs(2),
            new_client_block_timeout: Duration::from_secs(2),
            enable_websocket_echo: true,
            websocket_echo_interval: Duration::from_secs(24 * 60 * 60),
            websocket_echo_timeout: Duration::from_secs(5 * 60),
            disable_background_poller: false,
            allow_refresh_override: true,
            backoff_config: BackoffConfig::default(),
            enforce_go_limits: true,
        }
    }
}

impl ServiceConfig {
    /// Applies Go-aligned safety limits to runtime configuration settings.
    ///
    /// Hosts rarely want to diverge from the Go defaults; this method clamps
    /// refresh/bypass/client TTL values and toggles `allow_refresh_override`
    /// the same way the Go agent does so parity is maintained automatically.
    pub(crate) fn sanitise(mut self) -> Self {
        if !self.enforce_go_limits {
            return self;
        }

        // Refresh interval (baseline) – enforce minimum and toggle override flag when customised.
        if self.default_refresh_interval < MIN_REFRESH_INTERVAL {
            warn!(
                "default refresh interval {:?} is below the minimum {:?}; using {:?} instead",
                self.default_refresh_interval, MIN_REFRESH_INTERVAL, DEFAULT_REFRESH_INTERVAL
            );
            self.default_refresh_interval = DEFAULT_REFRESH_INTERVAL;
            self.allow_refresh_override = true;
        } else if self.default_refresh_interval != DEFAULT_REFRESH_INTERVAL {
            self.allow_refresh_override = false;
        }

        // Minimum refresh interval should not exceed the baseline and must honour the minimum.
        if self.min_refresh_interval < MIN_REFRESH_INTERVAL {
            warn!(
                "min refresh interval {:?} is below the minimum {:?}; clamping",
                self.min_refresh_interval, MIN_REFRESH_INTERVAL
            );
            self.min_refresh_interval = MIN_REFRESH_INTERVAL;
        }
        if self.min_refresh_interval > self.default_refresh_interval {
            warn!(
                "min refresh interval {:?} exceeds default refresh interval {:?}; aligning with default",
                self.min_refresh_interval, self.default_refresh_interval
            );
            self.min_refresh_interval = self.default_refresh_interval;
        }

        // Organisation status polling cadence mirrors refresh interval constraints.
        if self.org_status_interval < MIN_REFRESH_INTERVAL {
            warn!(
                "org status interval {:?} is below the minimum {:?}; using {:?}",
                self.org_status_interval, MIN_REFRESH_INTERVAL, DEFAULT_ORG_STATUS_INTERVAL
            );
            self.org_status_interval = DEFAULT_REFRESH_INTERVAL;
        }

        // Cache-bypass limit must live within [1, 10].
        if self.cache_bypass_limit < MIN_CACHE_BYPASS_LIMIT
            || self.cache_bypass_limit > MAX_CACHE_BYPASS_LIMIT
        {
            warn!(
                "cache bypass limit {} outside range {}-{}; using default {}",
                self.cache_bypass_limit,
                MIN_CACHE_BYPASS_LIMIT,
                MAX_CACHE_BYPASS_LIMIT,
                DEFAULT_CACHE_BYPASS_LIMIT
            );
            self.cache_bypass_limit = DEFAULT_CACHE_BYPASS_LIMIT;
        }

        // Client TTL must be sensible to avoid stale descriptors lingering indefinitely.
        if self.clients_ttl < MIN_REFRESH_INTERVAL || self.clients_ttl > MAX_CLIENTS_TTL {
            warn!(
                "client TTL {:?} outside range {:?}-{:?}; using default {:?}",
                self.clients_ttl, MIN_REFRESH_INTERVAL, MAX_CLIENTS_TTL, DEFAULT_CLIENTS_TTL
            );
            self.clients_ttl = DEFAULT_CLIENTS_TTL;
        }

        if self.new_client_block_timeout.is_zero() {
            warn!(
                "new client bypass timeout must be > 0; defaulting to {:?}",
                Duration::from_secs(2)
            );
            self.new_client_block_timeout = Duration::from_secs(2);
        }

        // Clamp backoff max interval to the permitted bounds.
        if self.backoff_config.max_backoff < MIN_BACKOFF_INTERVAL {
            warn!(
                "backoff max {:?} below minimum {:?}; raising to {:?}",
                self.backoff_config.max_backoff, MIN_BACKOFF_INTERVAL, MIN_BACKOFF_INTERVAL
            );
            self.backoff_config.max_backoff = MIN_BACKOFF_INTERVAL;
        } else if self.backoff_config.max_backoff > MAX_BACKOFF_INTERVAL {
            warn!(
                "backoff max {:?} above maximum {:?}; reducing to {:?}",
                self.backoff_config.max_backoff, MAX_BACKOFF_INTERVAL, MAX_BACKOFF_INTERVAL
            );
            self.backoff_config.max_backoff = MAX_BACKOFF_INTERVAL;
        }

        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensures refresh interval fields are clamped to the Go-aligned defaults.
    #[test]
    fn sanitise_clamps_refresh_intervals() {
        let config = ServiceConfig {
            default_refresh_interval: Duration::from_secs(1),
            min_refresh_interval: Duration::from_secs(1),
            org_status_interval: Duration::from_secs(1),
            ..Default::default()
        }
        .sanitise();
        assert_eq!(config.default_refresh_interval, DEFAULT_REFRESH_INTERVAL);
        assert_eq!(config.min_refresh_interval, MIN_REFRESH_INTERVAL);
        assert_eq!(config.org_status_interval, DEFAULT_REFRESH_INTERVAL);
        assert!(config.allow_refresh_override, "override flag should reset");
    }

    /// Verifies cache bypass, client TTL, and new-client timeout clamps are enforced.
    #[test]
    fn sanitise_limits_cache_and_client_windows() {
        let config = ServiceConfig {
            cache_bypass_limit: 99,
            clients_ttl: Duration::from_secs(1),
            new_client_block_timeout: Duration::from_secs(0),
            backoff_config: BackoffConfig {
                max_backoff: Duration::from_secs(10),
                ..Default::default()
            },
            ..Default::default()
        }
        .sanitise();
        assert_eq!(config.cache_bypass_limit, DEFAULT_CACHE_BYPASS_LIMIT);
        assert_eq!(config.clients_ttl, DEFAULT_CLIENTS_TTL);
        assert_eq!(config.new_client_block_timeout, Duration::from_secs(2));
        assert_eq!(config.backoff_config.max_backoff, MIN_BACKOFF_INTERVAL);
    }
}
