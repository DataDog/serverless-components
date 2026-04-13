// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Shared tag builder for enhanced metrics.
//!
//! Tags are attached to all enhanced metrics submitted by the metrics collector.

use dogstatsd::metric::SortedTags;
use libdd_common::azure_app_services;
use std::env;

/// Builds the common tags for all enhanced metrics.
///
/// Sources:
/// - Azure metadata (resource_group, subscription_id, name) from libdd_common
/// - Environment variables (region, plan_tier, service, env, version, serverless_compat_version)
///
/// The DogStatsD origin tag (e.g. `origin:azurefunction`) is added by the metrics aggregator,
/// not here.
pub fn build_enhanced_metrics_tags() -> Option<SortedTags> {
    let mut tag_parts = Vec::new();

    if let Some(aas_metadata) = &*azure_app_services::AAS_METADATA_FUNCTION {
        let aas_tags = [
            ("resource_group", aas_metadata.get_resource_group()),
            ("subscription_id", aas_metadata.get_subscription_id()),
            ("name", aas_metadata.get_site_name()),
        ];
        for (name, value) in aas_tags {
            if value != "unknown" {
                tag_parts.push(format!("{}:{}", name, value));
            }
        }
    }

    for (tag_name, env_var) in [
        ("region", "REGION_NAME"),
        ("plan_tier", "WEBSITE_SKU"),
        ("service", "DD_SERVICE"),
        ("env", "DD_ENV"),
        ("version", "DD_VERSION"),
        ("serverless_compat_version", "DD_SERVERLESS_COMPAT_VERSION"),
    ] {
        if let Ok(val) = env::var(env_var)
            && !val.is_empty()
        {
            tag_parts.push(format!("{}:{}", tag_name, val));
        }
    }

    if tag_parts.is_empty() {
        return None;
    }
    SortedTags::parse(&tag_parts.join(",")).ok()
}
