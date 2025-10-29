//! Environment-driven helpers for bootstrapping remote configuration.
//!
//! This module provides utilities to derive crate configuration from the host
//! process environment. It mirrors the defaults used by the Go implementation
//! (remote configuration enabled by default, `config.<site>` endpoint, and TLS
//! preference) while remaining embedder-agnostic.

use std::collections::HashMap;
use std::env;

use crate::http::Auth;

/// Name of the environment variable toggling remote configuration.
const ENV_ENABLED: &str = "DD_REMOTE_CONFIGURATION_ENABLED";
/// Primary environment variable carrying a dedicated remote configuration API key.
const ENV_RC_API_KEY: &str = "DD_REMOTE_CONFIGURATION_API_KEY";
/// Fallback environment variable for the general Datadog API key.
const ENV_API_KEY: &str = "DD_API_KEY";
/// Environment variable setting the remote configuration key.
const ENV_RC_KEY: &str = "DD_REMOTE_CONFIGURATION_KEY";
/// Environment variable providing a PAR JWT token (private action runner support).
const ENV_PAR_JWT: &str = "DD_REMOTE_CONFIGURATION_PAR_JWT";
/// Environment variable overriding the embedded config root metadata.
const ENV_CONFIG_ROOT: &str = "DD_REMOTE_CONFIGURATION_CONFIG_ROOT";
/// Environment variable overriding the embedded director root metadata.
const ENV_DIRECTOR_ROOT: &str = "DD_REMOTE_CONFIGURATION_DIRECTOR_ROOT";
/// Environment variable defining the Datadog site (e.g., `datadoghq.com`).
const ENV_SITE: &str = "DD_SITE";
/// Environment variable toggling TLS usage for remote configuration calls.
const ENV_NO_TLS: &str = "DD_REMOTE_CONFIGURATION_NO_TLS";
/// Environment variable toggling TLS certificate validation.
const ENV_NO_TLS_VALIDATION: &str = "DD_REMOTE_CONFIGURATION_NO_TLS_VALIDATION";
/// Experimental environment toggle forcing Remote Config to remain disabled.
const ENV_DISABLED: &str = "DD_REMOTE_CONFIGURATION_FORCE_OFF";

/// Default Datadog site used when none is supplied.
const DEFAULT_SITE: &str = "datadoghq.com";

/// Captures environment-derived options used to bootstrap the remote configuration client.
#[derive(Debug, Clone)]
pub struct RemoteConfigEnv {
    /// Whether remote configuration is enabled. Defaults to `true`.
    pub enabled: bool,
    /// Sanitised API key used for authentication (falls back to `DD_API_KEY`).
    pub api_key: Option<String>,
    /// Optional legacy remote configuration key.
    pub rc_key: Option<String>,
    /// Optional Private Action Runner JWT.
    pub par_jwt: Option<String>,
    /// Optional override for the embedded config root metadata.
    pub config_root_override: Option<String>,
    /// Optional override for the embedded director root metadata.
    pub director_root_override: Option<String>,
    /// Datadog site (e.g., `datadoghq.com`, `datadoghq.eu`).
    pub site: String,
    /// When `true`, HTTP (instead of HTTPS) should be used for requests.
    pub no_tls: bool,
    /// When `true`, TLS certificate validation should be skipped.
    pub no_tls_validation: bool,
}

impl RemoteConfigEnv {
    /// Builds settings from the current process environment.
    ///
    /// This function is side-effect free apart from reading `std::env::vars`.
    /// Embedder-specific defaults can be applied afterwards before constructing
    /// the HTTP client or service.
    pub fn from_os_env() -> Self {
        Self::from_env_iter(env::vars())
    }

    /// Builds settings from an iterator of key/value pairs (typically for tests).
    pub fn from_env_iter<I, K, V>(iter: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let map: HashMap<String, String> = iter
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();

        let enabled_default = parse_bool(map.get(ENV_ENABLED).map(String::as_str), true);
        let forced_off = parse_bool(map.get(ENV_DISABLED).map(String::as_str), false);
        // Honour the force-off gate even if the main toggle is true.
        let enabled = enabled_default && !forced_off;
        let api_key = map
            .get(ENV_RC_API_KEY)
            // Prefer the dedicated remote configuration key when present.
            .and_then(|value| sanitize_api_key(value))
            .or_else(|| {
                map.get(ENV_API_KEY)
                    .and_then(|value| sanitize_api_key(value))
            });
        let rc_key = map
            .get(ENV_RC_KEY)
            // Legacy RC key is optional; ignore empty strings to avoid bad headers.
            .and_then(|value| sanitize_non_empty(value));
        let par_jwt = map
            .get(ENV_PAR_JWT)
            // Private Action Runner tokens are optional and may not be present on SaaS installs.
            .and_then(|value| sanitize_non_empty(value));
        let config_root_override = map
            .get(ENV_CONFIG_ROOT)
            .and_then(|value| sanitize_non_empty(value));
        let director_root_override = map
            .get(ENV_DIRECTOR_ROOT)
            .and_then(|value| sanitize_non_empty(value));
        let site = map
            .get(ENV_SITE)
            .and_then(|value| sanitize_non_empty(value))
            .unwrap_or_else(|| DEFAULT_SITE.to_string());
        let no_tls = parse_bool(map.get(ENV_NO_TLS).map(String::as_str), false);
        let no_tls_validation =
            parse_bool(map.get(ENV_NO_TLS_VALIDATION).map(String::as_str), false);

        Self {
            enabled,
            api_key,
            rc_key,
            par_jwt,
            config_root_override,
            director_root_override,
            site,
            no_tls,
            no_tls_validation,
        }
    }

    /// Builds the remote configuration base URL using the captured site and TLS preference.
    pub fn base_url(&self) -> String {
        let scheme = if self.no_tls { "http" } else { "https" };
        format!("{scheme}://config.{}", self.site)
    }

    /// Builds an [`Auth`] structure when API key material is available.
    pub fn to_auth(&self) -> Option<Auth> {
        self.api_key.as_ref().map(|api_key| Auth {
            api_key: api_key.clone(),
            application_key: None,
            par_jwt: self.par_jwt.clone(),
        })
    }
}

/// Returns a sanitised API key (trims whitespace) or `None` if the input is blank.
pub fn sanitize_api_key(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Helper trimming whitespace and discarding empty values.
fn sanitize_non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Parses boolean values from strings, falling back to the provided default.
fn parse_bool(value: Option<&str>, default: bool) -> bool {
    match value.map(|s| s.trim().to_ascii_lowercase()) {
        // Accept the common set of truthy strings.
        Some(ref v) if ["1", "true", "t", "yes", "y"].contains(&v.as_str()) => true,
        // Accept the common set of falsy strings.
        Some(ref v) if ["0", "false", "f", "no", "n"].contains(&v.as_str()) => false,
        // Fall back to the supplied default when the input is absent or ambiguous.
        _ => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensures defaults match the expected enablement and site settings.
    #[test]
    fn remote_config_env_defaults() {
        let env = RemoteConfigEnv::from_env_iter::<Vec<(String, String)>, _, _>(vec![]);
        assert!(env.enabled);
        assert_eq!(env.site, DEFAULT_SITE);
        assert_eq!(env.base_url(), format!("https://config.{DEFAULT_SITE}"));
        assert!(!env.no_tls);
        assert!(!env.no_tls_validation);
        assert!(env.api_key.is_none());
    }

    /// Confirms boolean parsing honours common truthy/falsy spellings.
    #[test]
    fn parse_bool_permits_common_variants() {
        assert!(parse_bool(Some("true"), false));
        assert!(parse_bool(Some("Yes"), false));
        assert!(parse_bool(Some("1"), false));
        assert!(!parse_bool(Some("false"), true));
        assert!(!parse_bool(Some("0"), true));
        assert!(parse_bool(Some("maybe"), true));
    }

    /// Validates API key sanitisation removes whitespace and ignores blanks.
    #[test]
    fn sanitize_api_key_trims_and_filters_empty_values() {
        assert_eq!(sanitize_api_key("  key-123 \n").as_deref(), Some("key-123"));
        assert_eq!(sanitize_api_key("   "), None);
    }

    /// Confirms environment-derived settings respect overrides and fallbacks.
    #[test]
    fn remote_config_env_honours_overrides() {
        let env = RemoteConfigEnv::from_env_iter([
            (ENV_ENABLED, "false"),
            (ENV_RC_API_KEY, "  dedicated-key "),
            (ENV_RC_KEY, "rc-secret"),
            (ENV_PAR_JWT, "jwt-token"),
            (ENV_SITE, "datadoghq.eu"),
            (ENV_NO_TLS, "1"),
            (ENV_NO_TLS_VALIDATION, "true"),
        ]);
        assert!(!env.enabled);
        assert_eq!(env.api_key.as_deref(), Some("dedicated-key"));
        assert_eq!(env.rc_key.as_deref(), Some("rc-secret"));
        assert_eq!(env.par_jwt.as_deref(), Some("jwt-token"));
        assert_eq!(env.site, "datadoghq.eu");
        assert!(env.no_tls);
        assert!(env.no_tls_validation);
        assert_eq!(env.base_url(), "http://config.datadoghq.eu");
    }

    /// Verifies the fallback to `DD_API_KEY` when a dedicated key is absent.
    #[test]
    fn remote_config_env_falls_back_to_general_api_key() {
        let env = RemoteConfigEnv::from_env_iter([(ENV_API_KEY, " general-key ")]);
        assert_eq!(env.api_key.as_deref(), Some("general-key"));
    }

    /// Ensures the helper can materialise an `Auth` structure when credentials exist.
    #[test]
    fn remote_config_env_builds_auth() {
        let env = RemoteConfigEnv::from_env_iter([
            (ENV_API_KEY, "apikey"),
            (ENV_RC_KEY, "rc"),
            (ENV_PAR_JWT, "jwt"),
        ]);
        let auth = env.to_auth().expect("auth should be available");
        assert_eq!(auth.api_key, "apikey");
        assert!(auth.application_key.is_none());
        assert_eq!(auth.par_jwt.as_deref(), Some("jwt"));
    }
}
/// Ensures the force-off flag overrides the enabled toggle.
#[test]
fn force_off_overrides_enabled() {
    let env = RemoteConfigEnv::from_env_iter([(ENV_ENABLED, "true"), (ENV_DISABLED, "1")]);
    assert!(!env.enabled);
}

/// Ensures the base_url() method uses the correct "config." subdomain
#[test]
fn base_url_uses_config_subdomain() {
    // Test with standard datadoghq.com site
    let env = RemoteConfigEnv {
        enabled: true,
        api_key: Some("test_key".to_string()),
        rc_key: None,
        par_jwt: None,
        config_root_override: None,
        director_root_override: None,
        site: "datadoghq.com".to_string(),
        no_tls: false,
        no_tls_validation: false,
    };
    assert_eq!(env.base_url(), "https://config.datadoghq.com");

    // Test with EU site
    let env_eu = RemoteConfigEnv {
        enabled: true,
        api_key: Some("test_key".to_string()),
        rc_key: None,
        par_jwt: None,
        config_root_override: None,
        director_root_override: None,
        site: "datadoghq.eu".to_string(),
        no_tls: false,
        no_tls_validation: false,
    };
    assert_eq!(env_eu.base_url(), "https://config.datadoghq.eu");

    // Test with no_tls enabled
    let env_no_tls = RemoteConfigEnv {
        enabled: true,
        api_key: Some("test_key".to_string()),
        rc_key: None,
        par_jwt: None,
        config_root_override: None,
        director_root_override: None,
        site: "datadoghq.com".to_string(),
        no_tls: true,
        no_tls_validation: false,
    };
    assert_eq!(env_no_tls.base_url(), "http://config.datadoghq.com");

    // Regression test: ensure we never use "api." subdomain for Remote Config
    assert!(!env.base_url().contains("://api."));
    assert!(!env_eu.base_url().contains("://api."));
    assert!(!env_no_tls.base_url().contains("://api."));
}
