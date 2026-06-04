// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Instance identity metric collector for Azure Functions.
//!
//! Submits `azure.functions.enhanced.instance` with value 1.0 on each
//! collection tick, tagged with the instance identifier.

use dogstatsd::aggregator::AggregatorHandle;
use dogstatsd::metric::{Metric, MetricValue, SortedTags};
use libdd_common::azure_app_services;
use tracing::{error, warn};

const INSTANCE_METRIC: &str = "azure.functions.enhanced.instance";

pub struct InstanceMetricsCollector {
    aggregator: AggregatorHandle,
    tags: Option<SortedTags>,
}

impl InstanceMetricsCollector {
    /// Creates a new collector, returning `None` if no instance ID is found.
    pub fn new(aggregator: AggregatorHandle, tags: Option<SortedTags>) -> Option<Self> {
        let instance_name = azure_app_services::AAS_METADATA_FUNCTION
            .as_ref()
            .map(|m| m.get_instance_name().to_lowercase())
            .filter(|n| n != azure_app_services::UNKNOWN_VALUE);

        let Some(instance_id) = instance_name else {
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
