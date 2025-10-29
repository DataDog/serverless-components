//! Container ID extraction and tag management
//!
//! This module provides functionality to extract container IDs from HTTP headers
//! and retrieve container tags for enriching proxy requests.

use axum::http::HeaderMap;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, trace};

use super::tagger::{EntityID, TagCardinality, Tagger};

/// LocalData structure parsed from the DD-LocalData header
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalData {
    #[serde(rename = "container-id")]
    pub container_id: Option<String>,
    #[serde(rename = "process-id")]
    pub process_id: Option<u32>,
    #[serde(rename = "entity-id")]
    pub entity_id: Option<String>,
}

/// ExternalData structure parsed from DD-ExternalData header
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalData {
    #[serde(rename = "container-id")]
    pub container_id: Option<String>,
    #[serde(rename = "pod-uid")]
    pub pod_uid: Option<String>,
}

/// Container ID provider for extracting container IDs from HTTP requests
#[derive(Clone)]
pub struct ContainerIDProvider {
    tagger: Arc<Tagger>,
}

impl ContainerIDProvider {
    pub fn new() -> Self {
        Self {
            tagger: Arc::new(Tagger::new()),
        }
    }

    pub fn new_with_tagger(tagger: Arc<Tagger>) -> Self {
        Self { tagger }
    }

    /// Extract container ID from HTTP headers
    ///
    /// Priority order (matching Go implementation):
    /// 1. DD-LocalData header (JSON with container-id field) + PID-based extraction
    /// 2. Datadog-Container-ID header (legacy)
    /// 3. DD-ExternalData header (JSON with container-id field)
    /// 4. PID-based cgroup extraction (if process_id available)
    pub fn get_container_id(&self, headers: &HeaderMap) -> Option<String> {
        // Try DD-LocalData header first (with PID-based extraction fallback)
        if let Some(local_data) = headers.get("dd-localdata") {
            if let Ok(local_data_str) = local_data.to_str() {
                match self.parse_local_data(local_data_str) {
                    Ok(data) => {
                        // If container ID is directly in LocalData, use it
                        if let Some(cid) = &data.container_id {
                            debug!("Extracted container ID from DD-LocalData: {}", cid);
                            return Some(cid.clone());
                        }

                        // Otherwise, try to generate from origin info (PID-based)
                        if let Some(cid) = self.tagger.generate_container_id_from_origin(&data) {
                            debug!("Generated container ID from LocalData origin info: {}", cid);
                            return Some(cid);
                        }
                    }
                    Err(e) => {
                        debug!("Failed to parse DD-LocalData: {}", e);
                    }
                }
            }
        }

        // Try Datadog-Container-ID header (legacy support)
        if let Some(container_id) = headers.get("datadog-container-id") {
            if let Ok(cid) = container_id.to_str() {
                debug!(
                    "Extracted container ID from Datadog-Container-ID header: {}",
                    cid
                );
                return Some(cid.to_string());
            }
        }

        // Try DD-ExternalData header
        if let Some(external_data) = headers.get("dd-externaldata") {
            if let Ok(external_data_str) = external_data.to_str() {
                match self.parse_external_data(external_data_str) {
                    Ok(data) => {
                        if let Some(cid) = data.container_id {
                            debug!("Extracted container ID from DD-ExternalData: {}", cid);
                            return Some(cid);
                        }
                    }
                    Err(e) => {
                        debug!("Failed to parse DD-ExternalData: {}", e);
                    }
                }
            }
        }

        trace!("No container ID found in headers");
        None
    }

    /// Parse DD-LocalData header (base64-encoded JSON)
    fn parse_local_data(&self, data: &str) -> Result<LocalData, String> {
        // Try direct JSON parse first
        if let Ok(parsed) = serde_json::from_str::<LocalData>(data) {
            return Ok(parsed);
        }

        // Try base64 decode then JSON parse
        if let Ok(decoded) = STANDARD.decode(data) {
            if let Ok(json_str) = String::from_utf8(decoded) {
                return serde_json::from_str::<LocalData>(&json_str)
                    .map_err(|e| format!("Failed to parse LocalData JSON: {}", e));
            }
        }

        Err("Failed to parse LocalData".to_string())
    }

    /// Parse DD-ExternalData header (base64-encoded JSON)
    fn parse_external_data(&self, data: &str) -> Result<ExternalData, String> {
        // Try direct JSON parse first
        if let Ok(parsed) = serde_json::from_str::<ExternalData>(data) {
            return Ok(parsed);
        }

        // Try base64 decode then JSON parse
        if let Ok(decoded) = STANDARD.decode(data) {
            if let Ok(json_str) = String::from_utf8(decoded) {
                return serde_json::from_str::<ExternalData>(&json_str)
                    .map_err(|e| format!("Failed to parse ExternalData JSON: {}", e));
            }
        }

        Err("Failed to parse ExternalData".to_string())
    }
}

/// Container tags provider
///
/// This integrates with the Datadog tagger to retrieve orchestrator tags
/// for containers (pod name, cluster name, namespace, deployment, etc.).
#[derive(Clone)]
pub struct ContainerTagsProvider {
    tagger: Arc<Tagger>,
    cardinality: TagCardinality,
}

impl ContainerTagsProvider {
    pub fn new() -> Self {
        Self {
            tagger: Arc::new(Tagger::new()),
            cardinality: TagCardinality::High,
        }
    }

    pub fn new_with_tagger(tagger: Arc<Tagger>) -> Self {
        Self {
            tagger,
            cardinality: TagCardinality::High,
        }
    }

    pub fn new_with_cardinality(tagger: Arc<Tagger>, cardinality: TagCardinality) -> Self {
        Self {
            tagger,
            cardinality,
        }
    }

    /// Get tags for a container ID
    ///
    /// Returns a list of tags associated with the container.
    /// Format: ["key:value", "key2:value2", ...]
    ///
    /// Tags retrieved from the tagger include:
    /// - container_id: The container ID
    /// - pod_name: Kubernetes pod name
    /// - kube_namespace: Kubernetes namespace
    /// - kube_deployment: Kubernetes deployment
    /// - cluster_name: Kubernetes cluster name
    /// - image_name: Container image name
    /// - image_tag: Container image tag
    pub fn get_tags(&self, container_id: &str) -> Vec<String> {
        if container_id.is_empty() {
            return Vec::new();
        }

        let entity = EntityID::container_id(container_id.to_string());
        let tags = self.tagger.tag(entity, self.cardinality);

        trace!(
            "Retrieved {} tags for container {}",
            tags.len(),
            container_id
        );
        tags
    }

    /// Format tags as a comma-separated string
    pub fn format_tags(&self, container_id: &str) -> Option<String> {
        let tags = self.get_tags(container_id);
        if tags.is_empty() {
            return None;
        }

        Some(tags.join(","))
    }
}

impl Default for ContainerIDProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for ContainerTagsProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn test_extract_container_id_from_datadog_header() {
        let provider = ContainerIDProvider::new();
        let mut headers = HeaderMap::new();
        headers.insert(
            "datadog-container-id",
            HeaderValue::from_static("abc123def456"),
        );

        let cid = provider.get_container_id(&headers);
        assert_eq!(cid, Some("abc123def456".to_string()));
    }

    #[test]
    fn test_extract_container_id_from_local_data_json() {
        let provider = ContainerIDProvider::new();
        let mut headers = HeaderMap::new();

        let local_data = r#"{"container-id":"xyz789"}"#;
        headers.insert("dd-localdata", HeaderValue::from_str(local_data).unwrap());

        let cid = provider.get_container_id(&headers);
        assert_eq!(cid, Some("xyz789".to_string()));
    }

    #[test]
    fn test_extract_container_id_from_local_data_base64() {
        let provider = ContainerIDProvider::new();
        let mut headers = HeaderMap::new();

        // base64 of: {"container-id":"base64test"}
        let local_data = "eyJjb250YWluZXItaWQiOiJiYXNlNjR0ZXN0In0=";
        headers.insert("dd-localdata", HeaderValue::from_str(local_data).unwrap());

        let cid = provider.get_container_id(&headers);
        assert_eq!(cid, Some("base64test".to_string()));
    }

    #[test]
    fn test_priority_order_local_data_over_header() {
        let provider = ContainerIDProvider::new();
        let mut headers = HeaderMap::new();

        // Both headers present - LocalData should take priority
        let local_data = r#"{"container-id":"from-localdata"}"#;
        headers.insert("dd-localdata", HeaderValue::from_str(local_data).unwrap());
        headers.insert(
            "datadog-container-id",
            HeaderValue::from_static("from-legacy-header"),
        );

        let cid = provider.get_container_id(&headers);
        assert_eq!(cid, Some("from-localdata".to_string()));
    }

    #[test]
    fn test_no_container_id() {
        let provider = ContainerIDProvider::new();
        let headers = HeaderMap::new();

        let cid = provider.get_container_id(&headers);
        assert_eq!(cid, None);
    }

    #[test]
    fn test_container_tags_basic() {
        let tags_provider = ContainerTagsProvider::new();
        let tags = tags_provider.get_tags("test-container-123");

        assert!(!tags.is_empty());
        assert!(tags.contains(&"container_id:test-container-123".to_string()));
    }

    #[test]
    fn test_container_tags_format() {
        let tags_provider = ContainerTagsProvider::new();
        let formatted = tags_provider.format_tags("test-container");

        assert!(formatted.is_some());
        assert!(formatted.unwrap().contains("container_id:test-container"));
    }

    #[test]
    fn test_container_tags_empty_container_id() {
        let tags_provider = ContainerTagsProvider::new();
        let tags = tags_provider.get_tags("");

        assert!(tags.is_empty());
    }
}
