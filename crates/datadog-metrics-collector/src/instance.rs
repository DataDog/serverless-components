// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Instance identity metric collector for Azure Functions.
//!
//! Submits `azure.functions.enhanced.instance` with value 1.0 on each
//! collection tick, tagged with the instance identifier.

use dogstatsd::aggregator::AggregatorHandle;
use dogstatsd::metric::{Metric, MetricValue, SortedTags};
use std::env;
use tracing::{error, warn};

const INSTANCE_METRIC: &str = "azure.functions.enhanced.instance";

/// Resolves the instance ID from explicit values (used by tests).
fn resolve_instance_id_from(
    website_instance_id: Option<&str>,
    website_pod_name: Option<&str>,
    container_name: Option<&str>,
) -> Option<String> {
    website_instance_id
        .or(website_pod_name)
        .or(container_name)
        .map(String::from)
}

/// Resolves the instance ID from environment variables.
///
/// Checks in order:
/// 1. `WEBSITE_INSTANCE_ID` (Elastic Premium / Premium plans)
/// 2. `WEBSITE_POD_NAME` (Flex Consumption / Consumption plans)
/// 3. `CONTAINER_NAME` (Flex Consumption / Consumption plans)
fn resolve_instance_id() -> Option<String> {
    resolve_instance_id_from(
        env::var("WEBSITE_INSTANCE_ID").ok().as_deref(),
        env::var("WEBSITE_POD_NAME").ok().as_deref(),
        env::var("CONTAINER_NAME").ok().as_deref(),
    )
}

pub struct InstanceMetricsCollector {
    aggregator: AggregatorHandle,
    tags: Option<SortedTags>,
}

impl InstanceMetricsCollector {
    /// Creates a new collector, returning `None` if no instance ID is found.
    pub fn new(aggregator: AggregatorHandle, tags: Option<SortedTags>) -> Option<Self> {
        let instance_id = resolve_instance_id();
        let Some(instance_id) = instance_id else {
            warn!("No instance ID found, instance metric will not be submitted");
            return None;
        };

        // Precompute tags: enhanced metrics tags + instance tag
        let instance_tag = format!("instance:{}", instance_id);
        let tags = match tags {
            Some(mut existing) => {
                if let Ok(id_tag) = SortedTags::parse(&instance_tag) {
                    existing.extend(&id_tag);
                }
                Some(existing)
            }
            None => SortedTags::parse(&instance_tag).ok(),
        };

        Some(Self { aggregator, tags })
    }

    pub fn collect_and_submit(&self) {
        let metric = Metric::new(
            INSTANCE_METRIC.into(),
            MetricValue::gauge(1.0),
            self.tags.clone(),
            None,
        );

        if let Err(e) = self.aggregator.insert_batch(vec![metric]) {
            error!("Failed to insert instance metric: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_instance_id_falls_back_to_pod_name() {
        let id = resolve_instance_id_from(None, Some("pod-xyz"), Some("container-123"));
        assert_eq!(id, Some("pod-xyz".to_string()));
    }

    #[test]
    fn test_resolve_instance_id_falls_back_to_container_name() {
        let id = resolve_instance_id_from(None, None, Some("container-123"));
        assert_eq!(id, Some("container-123".to_string()));
    }
}
