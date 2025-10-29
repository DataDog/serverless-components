//! Lightweight status/expvar helpers used to expose Remote Config health
//! (org enablement, authorization, and last error) to embedders.
//!
//! The Go agent publishes the same data via the `remoteConfigStatus` expvar.
//!
use serde_json::{Map, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Tracks the organisation enablement flags and the last error observed by
/// the Remote Config service.
#[derive(Debug)]
pub struct RemoteConfigStatus {
    org_enabled: AtomicBool,
    org_authorized: AtomicBool,
    last_error: RwLock<Option<String>>,
}

impl RemoteConfigStatus {
    /// Creates a reference-counted status handle with default (disabled)
    /// flags and no recorded error.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            org_enabled: AtomicBool::new(false),
            org_authorized: AtomicBool::new(false),
            last_error: RwLock::new(None),
        })
    }

    /// Returns whether the organisation has Remote Config enabled.
    pub fn org_enabled(&self) -> bool {
        self.org_enabled.load(Ordering::Relaxed)
    }

    /// Returns whether the current credentials are authorised for Remote Config.
    pub fn org_authorized(&self) -> bool {
        self.org_authorized.load(Ordering::Relaxed)
    }

    /// Returns the last refresh error recorded by the service (if any).
    pub async fn last_error(&self) -> Option<String> {
        self.last_error.read().await.clone()
    }

    /// Updates the boolean flags atomically.
    pub fn set_org_flags(&self, enabled: bool, authorized: bool) {
        self.org_enabled.store(enabled, Ordering::Relaxed);
        self.org_authorized.store(authorized, Ordering::Relaxed);
    }

    /// Stores the last refresh error message (or clears it when `None`).
    pub async fn set_last_error(&self, error: Option<String>) {
        let mut guard = self.last_error.write().await;
        *guard = error;
    }

    /// Returns a status snapshot suitable for logging/exporting.
    pub async fn snapshot(&self) -> StatusSnapshot {
        StatusSnapshot {
            org_enabled: self.org_enabled(),
            org_authorized: self.org_authorized(),
            last_error: self.last_error().await,
        }
    }

    /// Helper that renders the status flags as a JSON payload compatible with Go's expvar.
    pub async fn to_json(&self) -> Value {
        self.snapshot().await.to_json()
    }
}

/// Serializable representation of [`RemoteConfigStatus`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusSnapshot {
    pub org_enabled: bool,
    pub org_authorized: bool,
    pub last_error: Option<String>,
}

impl StatusSnapshot {
    /// Renders the snapshot as a JSON map matching the Go agent expvar.
    pub fn to_map(&self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("orgEnabled".into(), Value::Bool(self.org_enabled));
        map.insert("apiKeyScoped".into(), Value::Bool(self.org_authorized));
        map.insert(
            "lastError".into(),
            match &self.last_error {
                Some(err) => {
                    // Include the most recent error string so operators can
                    // triage issues without scraping logs.
                    Value::String(err.clone())
                }
                None => Value::Null,
            },
        );
        map
    }

    /// Wraps [`StatusSnapshot::to_map`] into a [`serde_json::Value`].
    pub fn to_json(&self) -> Value {
        Value::Object(self.to_map())
    }
}

/// Formats the current status as a JSON object (`{"orgEnabled":..,"apiKeyScoped":..,"lastError":..}`).
pub async fn status_json(status: &RemoteConfigStatus) -> Value {
    status.snapshot().await.to_json()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    /// Ensures the status handle consistently updates booleans and last-error fields.
    async fn status_handle_updates_fields() {
        let status = RemoteConfigStatus::new();
        status.set_org_flags(true, false);
        assert!(status.org_enabled());
        assert!(!status.org_authorized());
        status.set_last_error(Some("boom".into())).await;
        assert_eq!(status.last_error().await.as_deref(), Some("boom"));
        status.set_last_error(None).await;
        assert!(status.last_error().await.is_none());
    }

    #[tokio::test]
    /// Verifies the exported JSON mirrors the Go expvar layout.
    async fn status_snapshot_renders_json() {
        let status = RemoteConfigStatus::new();
        status.set_org_flags(true, true);
        status.set_last_error(Some("boom".into())).await;
        let json = status.to_json().await;
        assert_eq!(
            json,
            Value::Object(
                [
                    ("orgEnabled".into(), Value::Bool(true)),
                    ("apiKeyScoped".into(), Value::Bool(true)),
                    ("lastError".into(), Value::String("boom".into()))
                ]
                .into_iter()
                .collect()
            )
        );
    }
}
