//! Tagger module for container orchestrator tags
//!
//! This module provides container tagging functionality similar to the Go agent's tagger.
//! It retrieves orchestrator-level tags for containers (pod names, namespaces, cluster info, etc.)

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tracing::{debug, trace, warn};

/// Tag cardinality levels controlling which tags are returned.
///
/// Different cardinality levels balance between tag detail and cardinality cost.
/// Higher cardinality means more unique tag combinations and higher storage/processing costs.
///
/// This matches the Datadog Go agent's cardinality system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagCardinality {
    /// Low cardinality: cluster-level tags only.
    ///
    /// Examples:
    /// - `cluster_name:prod-us-east-1`
    /// - `kube_region:us-east-1`
    ///
    /// Use for high-volume metrics where cardinality must be minimized.
    Low,
    /// Orchestrator cardinality: namespace, deployment, service level tags.
    ///
    /// Examples (in addition to Low):
    /// - `kube_namespace:default`
    /// - `kube_deployment:my-app`
    /// - `kube_service:web-service`
    /// - `image_name:myapp`
    ///
    /// Use for most APM and metrics scenarios (default recommendation).
    Orchestrator,
    /// High cardinality: all tags including pod-level.
    ///
    /// Includes all tags from Low and Orchestrator, plus:
    /// - `pod_name:my-app-abc123`
    /// - `container_id:abc123def456`
    /// - `image_tag:v1.2.3`
    ///
    /// Use when you need maximum detail and cardinality cost is acceptable.
    High,
}

/// Entity identifier for tagging.
///
/// Represents a container, pod, or task that needs tags from the orchestrator.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EntityID {
    /// Type of entity (container, pod, or task).
    pub entity_type: EntityType,
    /// Unique identifier for the entity (container ID, pod UID, task ARN, etc.).
    pub id: String,
}

/// Type of orchestrator entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntityType {
    /// Container ID (Docker/containerd/cri-o).
    ///
    /// Example: `abc123def456789...` (64-char hex for Docker)
    ContainerID,
    /// Kubernetes Pod UID.
    ///
    /// Example: `550e8400-e29b-41d4-a716-446655440000`
    PodUID,
    /// ECS Task identifier.
    ///
    /// Example: Task ARN or task ID
    Task,
}

impl EntityID {
    pub fn new(entity_type: EntityType, id: String) -> Self {
        Self { entity_type, id }
    }

    pub fn container_id(id: String) -> Self {
        Self::new(EntityType::ContainerID, id)
    }
}

/// Container tags cache entry
#[derive(Debug, Clone)]
struct TagCacheEntry {
    tags: Vec<String>,
    // Future: Add TTL/expiry
}

/// Tagger for retrieving container orchestrator tags
///
/// In a full agent deployment, this would connect to:
/// - Docker API for container metadata
/// - Kubernetes API for pod/namespace/cluster info
/// - ECS Task metadata endpoint
/// - Container runtime sockets
///
/// Current implementation provides a mock/stub for testing and development.
pub struct Tagger {
    /// Cache of container ID -> tags mappings
    cache: Arc<RwLock<HashMap<String, TagCacheEntry>>>,
    /// Enable mock mode for testing
    mock_mode: bool,
}

impl Tagger {
    /// Create a new tagger instance
    pub fn new() -> Self {
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
            mock_mode: false,
        }
    }

    /// Create a tagger in mock mode (for testing)
    pub fn new_mock() -> Self {
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
            mock_mode: true,
        }
    }

    /// Add mock tags for testing
    pub fn add_mock_tags(&self, container_id: &str, tags: Vec<String>) {
        if let Ok(mut cache) = self.cache.write() {
            cache.insert(container_id.to_string(), TagCacheEntry { tags });
        }
    }

    /// Retrieve tags for a container entity
    ///
    /// This is the main entry point matching the Go agent's `tagger.Tag()` function.
    ///
    /// # Arguments
    /// * `entity` - The entity to get tags for
    /// * `cardinality` - The tag cardinality level
    ///
    /// # Returns
    /// Vector of tags in `key:value` format
    pub fn tag(&self, entity: EntityID, cardinality: TagCardinality) -> Vec<String> {
        trace!("Tagger::tag({:?}, {:?})", entity, cardinality);

        match entity.entity_type {
            EntityType::ContainerID => self.get_container_tags(&entity.id, cardinality),
            EntityType::PodUID => self.get_pod_tags(&entity.id, cardinality),
            EntityType::Task => self.get_task_tags(&entity.id, cardinality),
        }
    }

    /// Get tags for a container ID
    fn get_container_tags(&self, container_id: &str, cardinality: TagCardinality) -> Vec<String> {
        // Check cache first
        if let Ok(cache) = self.cache.read() {
            if let Some(entry) = cache.get(container_id) {
                debug!("Tagger cache hit for container {}", container_id);
                return self.filter_by_cardinality(&entry.tags, cardinality);
            }
        }

        // Cache miss - fetch tags
        let tags = if self.mock_mode {
            self.fetch_mock_tags(container_id)
        } else {
            self.fetch_container_tags(container_id)
        };

        // Store in cache
        if !tags.is_empty() {
            if let Ok(mut cache) = self.cache.write() {
                cache.insert(
                    container_id.to_string(),
                    TagCacheEntry { tags: tags.clone() },
                );
            }
        }

        self.filter_by_cardinality(&tags, cardinality)
    }

    /// Fetch container tags from orchestrator sources
    ///
    /// In production, this would:
    /// 1. Query Docker API for container metadata
    /// 2. Query Kubernetes API for pod/deployment/namespace info
    /// 3. Query ECS task metadata endpoint
    /// 4. Parse container runtime metadata
    fn fetch_container_tags(&self, container_id: &str) -> Vec<String> {
        // TODO: Implement real orchestrator queries
        // For now, return basic tags
        debug!(
            "Fetching tags for container {} (stub implementation)",
            container_id
        );

        vec![
            format!("container_id:{}", container_id),
            // Future: Add real orchestrator tags:
            // format!("pod_name:{}", pod_name),
            // format!("kube_namespace:{}", namespace),
            // format!("kube_deployment:{}", deployment),
            // format!("cluster_name:{}", cluster),
            // format!("image_name:{}", image),
            // format!("image_tag:{}", tag),
        ]
    }

    /// Fetch mock tags for testing
    fn fetch_mock_tags(&self, container_id: &str) -> Vec<String> {
        vec![
            format!("container_id:{}", container_id),
            format!(
                "pod_name:test-pod-{}",
                &container_id[..8.min(container_id.len())]
            ),
            "kube_namespace:default".to_string(),
            "kube_deployment:test-deployment".to_string(),
            "cluster_name:test-cluster".to_string(),
            "image_name:test-image".to_string(),
            "image_tag:latest".to_string(),
        ]
    }

    /// Get tags for a pod UID
    fn get_pod_tags(&self, _pod_uid: &str, _cardinality: TagCardinality) -> Vec<String> {
        // TODO: Implement pod-level tagging
        Vec::new()
    }

    /// Get tags for a task (ECS)
    fn get_task_tags(&self, _task_id: &str, _cardinality: TagCardinality) -> Vec<String> {
        // TODO: Implement ECS task tagging
        Vec::new()
    }

    /// Filter tags by cardinality level
    fn filter_by_cardinality(&self, tags: &[String], cardinality: TagCardinality) -> Vec<String> {
        match cardinality {
            TagCardinality::Low => {
                // Low cardinality: cluster-level tags only
                tags.iter()
                    .filter(|t| t.starts_with("cluster_name:") || t.starts_with("kube_region:"))
                    .cloned()
                    .collect()
            }
            TagCardinality::Orchestrator => {
                // Orchestrator: namespace, deployment, service level
                tags.iter()
                    .filter(|t| {
                        t.starts_with("cluster_name:")
                            || t.starts_with("kube_namespace:")
                            || t.starts_with("kube_deployment:")
                            || t.starts_with("kube_service:")
                            || t.starts_with("image_name:")
                    })
                    .cloned()
                    .collect()
            }
            TagCardinality::High => {
                // High cardinality: all tags including pod-level
                tags.to_vec()
            }
        }
    }

    /// Generate container ID from origin info (similar to Go agent)
    pub fn generate_container_id_from_origin(
        &self,
        local_data: &super::container::LocalData,
    ) -> Option<String> {
        // If container ID is directly provided, use it
        if let Some(ref cid) = local_data.container_id {
            return Some(cid.clone());
        }

        // If process ID is provided, try to extract container ID from cgroup
        if let Some(pid) = local_data.process_id {
            if let Some(cid) = self.extract_container_id_from_pid(pid) {
                return Some(cid);
            }
        }

        warn!("Could not generate container ID from origin info");
        None
    }

    /// Extract container ID from process cgroup
    fn extract_container_id_from_pid(&self, pid: u32) -> Option<String> {
        let cgroup_path = format!("/proc/{}/cgroup", pid);

        match std::fs::read_to_string(&cgroup_path) {
            Ok(content) => {
                debug!("Reading cgroup file for PID {}", pid);
                self.parse_container_id_from_cgroup(&content)
            }
            Err(e) => {
                trace!("Could not read cgroup file {}: {}", cgroup_path, e);
                None
            }
        }
    }

    /// Parse container ID from cgroup content
    ///
    /// Cgroup v1 format examples:
    /// - `12:pids:/docker/abc123...`
    /// - `1:name=systemd:/docker/abc123...`
    /// - `0::/system.slice/docker-abc123.scope`
    ///
    /// Cgroup v2 format:
    /// - `0::/system.slice/docker-abc123.scope`
    /// - `0::/kubepods/pod.../abc123`
    fn parse_container_id_from_cgroup(&self, content: &str) -> Option<String> {
        for line in content.lines() {
            // Docker container ID patterns
            if let Some(docker_id) = self.extract_docker_id(line) {
                return Some(docker_id);
            }

            // Kubernetes pod container ID patterns
            if let Some(k8s_id) = self.extract_kubernetes_id(line) {
                return Some(k8s_id);
            }

            // ECS task container ID patterns
            if let Some(ecs_id) = self.extract_ecs_id(line) {
                return Some(ecs_id);
            }
        }

        None
    }

    /// Extract Docker container ID from cgroup line
    fn extract_docker_id(&self, line: &str) -> Option<String> {
        // Pattern: /docker/CONTAINER_ID
        if let Some(pos) = line.find("/docker/") {
            let id_start = pos + "/docker/".len();
            let remaining = &line[id_start..];

            // Extract until next slash or end of line
            let id = remaining.split('/').next()?;

            // Docker IDs are 64 hex characters, but we'll accept 12+ for short IDs
            if id.len() >= 12 && id.chars().all(|c| c.is_ascii_hexdigit()) {
                return Some(id.to_string());
            }
        }

        // Pattern: docker-CONTAINER_ID.scope
        if let Some(pos) = line.find("docker-") {
            let id_start = pos + "docker-".len();
            if let Some(id_end) = line[id_start..].find(".scope") {
                let id = &line[id_start..id_start + id_end];
                if id.len() >= 12 && id.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Some(id.to_string());
                }
            }
        }

        None
    }

    /// Extract Kubernetes container ID from cgroup line
    fn extract_kubernetes_id(&self, line: &str) -> Option<String> {
        // Pattern: /kubepods/.../pod.../CONTAINER_ID
        if line.contains("/kubepods/") {
            // Split by / and get the last component
            let parts: Vec<&str> = line.split('/').collect();
            if let Some(last) = parts.last() {
                if last.len() >= 12 && last.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Some(last.to_string());
                }
            }
        }

        None
    }

    /// Extract ECS container ID from cgroup line
    fn extract_ecs_id(&self, line: &str) -> Option<String> {
        // Pattern: /ecs/TASK_ID/CONTAINER_ID
        if let Some(pos) = line.find("/ecs/") {
            let remaining = &line[pos + "/ecs/".len()..];
            let parts: Vec<&str> = remaining.split('/').collect();

            // Usually the second part after /ecs/ is the container ID
            if parts.len() >= 2 {
                let id = parts[1];
                if id.len() >= 12 && id.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Some(id.to_string());
                }
            }
        }

        None
    }
}

impl Default for Tagger {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tagger_basic() {
        let tagger = Tagger::new_mock();
        let entity = EntityID::container_id("test-container-123".to_string());
        let tags = tagger.tag(entity, TagCardinality::High);

        assert!(!tags.is_empty());
        assert!(tags.iter().any(|t| t.contains("container_id")));
    }

    #[test]
    fn test_tagger_cardinality_filtering() {
        let tagger = Tagger::new_mock();
        let entity = EntityID::container_id("test-container-123".to_string());

        let low_tags = tagger.tag(entity.clone(), TagCardinality::Low);
        let orch_tags = tagger.tag(entity.clone(), TagCardinality::Orchestrator);
        let high_tags = tagger.tag(entity, TagCardinality::High);

        // High cardinality should have most tags
        assert!(high_tags.len() >= orch_tags.len());
        assert!(orch_tags.len() >= low_tags.len());
    }

    #[test]
    fn test_parse_docker_cgroup_v1() {
        let tagger = Tagger::new();
        let cgroup = "12:pids:/docker/abc123def456789012345678901234567890123456789012345678901234";

        let container_id = tagger.parse_container_id_from_cgroup(cgroup);
        assert_eq!(
            container_id,
            Some("abc123def456789012345678901234567890123456789012345678901234".to_string())
        );
    }

    #[test]
    fn test_parse_docker_cgroup_scope() {
        let tagger = Tagger::new();
        let cgroup = "0::/system.slice/docker-abc123def456.scope";

        let container_id = tagger.parse_container_id_from_cgroup(cgroup);
        assert_eq!(container_id, Some("abc123def456".to_string()));
    }

    #[test]
    fn test_parse_kubernetes_cgroup() {
        let tagger = Tagger::new();
        let cgroup = "0::/kubepods/besteffort/pod123/abc123def456";

        let container_id = tagger.parse_container_id_from_cgroup(cgroup);
        assert_eq!(container_id, Some("abc123def456".to_string()));
    }

    #[test]
    fn test_tagger_cache() {
        let tagger = Tagger::new_mock();
        let entity = EntityID::container_id("cached-container".to_string());

        // First call - cache miss
        let tags1 = tagger.tag(entity.clone(), TagCardinality::High);

        // Second call - cache hit
        let tags2 = tagger.tag(entity, TagCardinality::High);

        assert_eq!(tags1, tags2);
    }

    #[test]
    fn test_add_mock_tags() {
        let tagger = Tagger::new_mock();
        let custom_tags = vec![
            "custom_tag:value1".to_string(),
            "another_tag:value2".to_string(),
        ];

        tagger.add_mock_tags("test-container", custom_tags.clone());

        let entity = EntityID::container_id("test-container".to_string());
        let tags = tagger.tag(entity, TagCardinality::High);

        assert!(tags.contains(&"custom_tag:value1".to_string()));
        assert!(tags.contains(&"another_tag:value2".to_string()));
    }
}
