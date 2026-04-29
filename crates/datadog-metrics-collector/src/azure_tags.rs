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
    let mut pairs: Vec<(&'static str, String)> = Vec::new();

    if let Some(aas_metadata) = &*azure_app_services::AAS_METADATA_FUNCTION {
        for (name, value) in [
            ("resource_group", aas_metadata.get_resource_group()),
            ("subscription_id", aas_metadata.get_subscription_id()),
            ("name", aas_metadata.get_site_name()),
        ] {
            if value != AAS_UNKNOWN_VALUE {
                pairs.push((name, value.to_string()));
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
            pairs.push((tag_name, val));
        }
    }

    build_tags(pairs)
}

fn build_tags(pairs: impl IntoIterator<Item = (&'static str, String)>) -> Option<SortedTags> {
    let mut tags: Vec<Tag> = Vec::new();
    for (key, value) in pairs {
        if value.is_empty() {
            continue;
        }
        // Tag::new validates the combined "key:value" string: it must be
        // non-empty and not start or end with a colon
        match Tag::new(key, &value) {
            Ok(t) => tags.push(t),
            Err(e) => warn!("Skipping invalid tag {key}:{value}: {e}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_tags_returns_none_when_no_pairs() {
        let pairs: Vec<(&'static str, String)> = Vec::new();
        assert!(build_tags(pairs).is_none());
    }

    #[test]
    fn test_build_tags_returns_none_when_all_values_empty() {
        let pairs = vec![("service", String::new()), ("env", String::new())];
        assert!(build_tags(pairs).is_none());
    }

    #[test]
    fn test_build_tags_skips_empty_values() {
        let pairs = vec![("service", String::new()), ("env", "dev".to_string())];
        let tags = build_tags(pairs).unwrap().to_strings();
        assert_eq!(tags, vec!["env:dev"]);
    }

    #[test]
    fn test_build_tags_includes_all_nonempty_pairs() {
        let pairs = vec![
            ("service", "svc-1".to_string()),
            ("env", "dev".to_string()),
            ("version", "1.2.3".to_string()),
        ];
        let mut tags = build_tags(pairs).unwrap().to_strings();
        tags.sort();
        assert_eq!(tags, vec!["env:dev", "service:svc-1", "version:1.2.3"]);
    }

    #[test]
    fn test_build_tags_rejects_trailing_colon_values() {
        let pairs = vec![
            ("service", "svc-1:".to_string()),
            ("env", "dev".to_string()),
        ];
        let tags = build_tags(pairs).unwrap().to_strings();
        assert_eq!(tags, vec!["env:dev"]);
    }
}
