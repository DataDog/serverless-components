//! Shared helpers for classifying and parsing Remote Config target paths.
//!
//! The Go agent exposes identical routines so both the service layer and the
//! Uptane verifier can reason about `datadog/<org>/<product>/…` and
//! `employee/<product>/…` layouts without duplicating logic.

/// Enumerates the recognised Remote Config namespaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigPathSource {
    /// Datadog-managed configs: `datadog/<org>/<product>/<config>/<file>`.
    Datadog,
    /// Internal employee configs: `employee/<product>/<config>/<file>`.
    Employee,
    /// Unknown prefixes are rejected and surfaced as errors.
    Unknown,
}

/// Structured view of a Remote Config target path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPathInfo {
    /// Product segment extracted from the path.
    pub product: String,
    /// Source namespace (`datadog`, `employee`, etc).
    pub source: ConfigPathSource,
    /// Optional org ID (present for Datadog-managed configs).
    pub org_id: Option<u64>,
}

/// Classifies a target path by inspecting its leading segment.
pub fn classify_config_path(path: &str) -> ConfigPathSource {
    if path.starts_with("datadog/") {
        return ConfigPathSource::Datadog;
    }
    if path.starts_with("employee/") {
        return ConfigPathSource::Employee;
    }
    ConfigPathSource::Unknown
}

/// Parses a target path and returns its structured representation.
pub fn parse_config_path(path: &str) -> Result<ConfigPathInfo, String> {
    match classify_config_path(path) {
        ConfigPathSource::Datadog => parse_datadog_config_path(path),
        ConfigPathSource::Employee => parse_employee_config_path(path),
        ConfigPathSource::Unknown => Err(format!("config path '{path}' has unknown source")),
    }
}

/// Parses a target path but returns `None` instead of surfacing an error.
pub fn parse_config_path_opt(path: &str) -> Option<ConfigPathInfo> {
    parse_config_path(path).ok()
}

/// Parses the Datadog-managed `datadog/<org>/<product>/<config>/<file>` format.
pub fn parse_datadog_config_path(path: &str) -> Result<ConfigPathInfo, String> {
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() != 5 || segments[0] != "datadog" {
        return Err(format!("config file path '{path}' has wrong format"));
    }
    let org_segment = segments[1];
    let org_id: u64 = org_segment.parse().map_err(|err| {
        format!("could not parse orgID '{org_segment}' in config file path: {err}")
    })?;
    let product_segment = segments[2];
    if product_segment.is_empty() {
        return Err("product is empty".to_string());
    }
    Ok(ConfigPathInfo {
        product: product_segment.to_string(),
        source: ConfigPathSource::Datadog,
        org_id: Some(org_id),
    })
}

/// Parses the internal `employee/<product>/<config>/<file>` format.
pub fn parse_employee_config_path(path: &str) -> Result<ConfigPathInfo, String> {
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() != 4 || segments[0] != "employee" {
        return Err(format!("config file path '{path}' has wrong format"));
    }
    let product_segment = segments[1];
    if product_segment.is_empty() {
        return Err("product is empty".to_string());
    }
    Ok(ConfigPathInfo {
        product: product_segment.to_string(),
        source: ConfigPathSource::Employee,
        org_id: None,
    })
}
