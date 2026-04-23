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
///
/// Picks the env var that matches the Azure integration metric's `instance`
/// tag for the current hosting plan (via `WEBSITE_SKU`), with fallback logic
/// if the preferred var is empty.
fn resolve_instance_id_from(
    website_sku: Option<&str>,
    container_name: Option<&str>,
    website_pod_name: Option<&str>,
    computer_name: Option<&str>,
) -> Option<String> {
    fn non_empty(s: Option<&str>) -> Option<&str> {
        s.filter(|v| !v.is_empty())
    }

    let sku_preferred = match website_sku {
        Some("FlexConsumption") | Some("Dynamic") => {
            non_empty(container_name).or(non_empty(website_pod_name))
        }
        Some(_) => non_empty(computer_name),
        None => None,
    };

    sku_preferred
        .or_else(|| non_empty(container_name))
        .or_else(|| non_empty(website_pod_name))
        .or_else(|| non_empty(computer_name))
        .map(|s| s.to_lowercase())
}

/// Resolves the instance ID from environment variables.
fn resolve_instance_id() -> Option<String> {
    resolve_instance_id_from(
        env::var("WEBSITE_SKU").ok().as_deref(),
        env::var("CONTAINER_NAME").ok().as_deref(),
        env::var("WEBSITE_POD_NAME").ok().as_deref(),
        env::var("COMPUTERNAME").ok().as_deref(),
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
    fn test_flex_consumption_uses_container_name() {
        let id = resolve_instance_id_from(
            Some("FlexConsumption"),
            Some("0--abc-DEF"),
            Some("0--abc-DEF"),
            None,
        );
        assert_eq!(id, Some("0--abc-def".to_string()));
    }

    #[test]
    fn test_flex_consumption_falls_back_to_pod_name_if_container_missing() {
        let id = resolve_instance_id_from(Some("FlexConsumption"), None, Some("pod-XYZ"), None);
        assert_eq!(id, Some("pod-xyz".to_string()));
    }

    #[test]
    fn test_consumption_uses_container_name() {
        let id = resolve_instance_id_from(
            Some("Dynamic"),
            Some("ABCD1234-111122223333444455"),
            None,
            None,
        );
        assert_eq!(id, Some("abcd1234-111122223333444455".to_string()));
    }

    #[test]
    fn test_elastic_premium_uses_computer_name() {
        let id =
            resolve_instance_id_from(Some("ElasticPremium"), None, None, Some("ep0fakewk0000A1"));
        assert_eq!(id, Some("ep0fakewk0000a1".to_string()));
    }

    #[test]
    fn test_dedicated_uses_computer_name() {
        let id = resolve_instance_id_from(Some("PremiumV3"), None, None, Some("p3fakewk0000B2"));
        assert_eq!(id, Some("p3fakewk0000b2".to_string()));
    }

    #[test]
    fn test_empty_string_is_treated_as_missing() {
        // EP2 .NET exposes WEBSITE_INSTANCE_ID as empty; we should skip empties.
        let id =
            resolve_instance_id_from(Some("ElasticPremium"), Some(""), Some(""), Some("worker-1"));
        assert_eq!(id, Some("worker-1".to_string()));
    }

    #[test]
    fn test_unknown_sku_falls_back_to_search_order() {
        let id = resolve_instance_id_from(Some("SomeNewSku"), Some("container-1"), None, None);
        assert_eq!(id, Some("container-1".to_string()));
    }

    #[test]
    fn test_missing_sku_falls_back_to_search_order() {
        let id = resolve_instance_id_from(None, Some("container-1"), None, Some("worker-1"));
        assert_eq!(id, Some("container-1".to_string()));
    }

    #[test]
    fn test_no_env_vars_returns_none() {
        let id = resolve_instance_id_from(None, None, None, None);
        assert_eq!(id, None);
    }
}
