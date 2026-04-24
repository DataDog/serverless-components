// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Shared tag builder for enhanced metrics.
//!
//! Tags are attached to all enhanced metrics submitted by the metrics collector.

use dogstatsd::metric::SortedTags;
use libdd_common::{azure_app_services, tag::Tag};
use std::env;
use tracing::warn;

/// `libdd_common::azure_app_services` returns this value when the corresponding Azure metadata isn't populated.
const AAS_UNKNOWN_VALUE: &str = "unknown";

/// Builds the common tags for all enhanced metrics.
///
/// Sources:
/// - Azure metadata (resource_group, subscription_id, name) from libdd_common
/// - Environment variables (region, plan_tier, service, env, version, serverless_compat_version)
///
/// The DogStatsD origin tag (e.g. `origin:azurefunction`) is added by the metrics aggregator,
/// not here.
pub fn build_enhanced_metrics_tags() -> Option<SortedTags> {
    let mut tags = Vec::new();

    fn push(tags: &mut Vec<Tag>, key: &str, value: &str) {
        if value.is_empty() {
            return;
        }
        // Tag::new validates that the key and value are not empty and do not start or end with a colon
        match Tag::new(key, value) {
            Ok(t) => tags.push(t),
            Err(e) => warn!("Skipping invalid tag {key}:{value}: {e}"),
        }
    }

    if let Some(aas_metadata) = &*azure_app_services::AAS_METADATA_FUNCTION {
        for (name, value) in [
            ("resource_group", aas_metadata.get_resource_group()),
            ("subscription_id", aas_metadata.get_subscription_id()),
            ("name", aas_metadata.get_site_name()),
        ] {
            if value != AAS_UNKNOWN_VALUE {
                push(&mut tags, name, value);
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
        if let Ok(val) = env::var(env_var) {
            push(&mut tags, tag_name, &val);
        }
    }

    if tags.is_empty() {
        return None;
    }
    let joined = tags
        .iter()
        .map(|t| t.as_ref())
        .collect::<Vec<&str>>()
        .join(",");
    SortedTags::parse(&joined).ok()
}
