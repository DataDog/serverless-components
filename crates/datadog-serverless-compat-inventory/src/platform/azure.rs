// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use super::PlatformData;
use crate::ProcessEnv;
use libdd_common::azure_app_services::{AzureMetadata, QueryEnv, UNKNOWN_VALUE};
use serde_json::{Map, Value};

fn known_value(value: &str) -> Option<&str> {
    (value != UNKNOWN_VALUE && !value.is_empty()).then_some(value)
}

pub(super) fn collect() -> Option<PlatformData> {
    collect_from(ProcessEnv)
}

fn collect_from<E: QueryEnv>(env: E) -> Option<PlatformData> {
    // These values are inventory-specific enrichments that are not exposed by
    // the published libdd-common 5.2 API used by this repository.
    let region = env
        .get_var("REGION_NAME")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let deployment_slot = env
        .get_var("WEBSITE_SLOT_NAME")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("production"));

    let azure = AzureMetadata::new_function(env)?;
    let resource_name = known_value(azure.get_site_name())?.to_string();
    let base_resource_id = known_value(azure.get_resource_id())?;
    let resource_id = match deployment_slot {
        Some(slot) => format!("{base_resource_id}/slots/{}", slot.to_lowercase()),
        None => base_resource_id.to_string(),
    };

    let mut metadata = Map::new();
    if let Some(region) = region {
        metadata.insert("region".into(), Value::String(region));
    }

    for (field, value) in [
        ("azure_subscription_id", azure.get_subscription_id()),
        ("azure_resource_group", azure.get_resource_group()),
        ("runtime", azure.get_runtime()),
        // This established downstream field name contains the application
        // runtime version (for example Python 3.13), not the Compat package.
        (
            "serverless_compat_runtime_version",
            azure.get_runtime_version(),
        ),
    ] {
        if let Some(value) = known_value(value) {
            metadata.insert(field.into(), Value::String(value.to_string()));
        }
    }

    Some(PlatformData {
        workload_type: "azure_function",
        resource_id,
        resource_name,
        metadata,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::FakeEnv;

    fn base_env() -> FakeEnv {
        FakeEnv::new(&[
            ("FUNCTIONS_WORKER_RUNTIME", "python"),
            ("WEBSITE_OWNER_NAME", "abc123+my-rg-eastuswebspace"),
            ("WEBSITE_RESOURCE_GROUP", "my-rg"),
            ("WEBSITE_SITE_NAME", "my-func-app"),
        ])
    }

    #[test]
    fn builds_full_identity() {
        let data = collect_from(base_env()).expect("Azure identity should be available");
        assert_eq!(data.resource_name, "my-func-app");
        assert_eq!(
            data.resource_id,
            "/subscriptions/abc123/resourcegroups/my-rg/providers/microsoft.web/sites/my-func-app"
        );
    }

    #[test]
    fn uses_flex_consumption_resource_group() {
        let data = collect_from(FakeEnv::new(&[
            ("FUNCTIONS_WORKER_RUNTIME", "python"),
            ("WEBSITE_OWNER_NAME", "abc123+flex-host"),
            ("WEBSITE_SITE_NAME", "my-flex-func"),
            ("WEBSITE_SKU", "FlexConsumption"),
            ("DD_AZURE_RESOURCE_GROUP", "My-Flex-RG"),
        ]))
        .expect("Flex identity should be available");
        assert!(data.resource_id.contains("/my-flex-rg/"));
    }

    #[test]
    fn missing_required_identity_skips_inventory() {
        let incomplete_environments = [
            FakeEnv::new(&[
                ("FUNCTIONS_WORKER_RUNTIME", "python"),
                ("WEBSITE_OWNER_NAME", "abc123+my-rg-eastuswebspace"),
                ("WEBSITE_RESOURCE_GROUP", "my-rg"),
            ]),
            FakeEnv::new(&[
                ("FUNCTIONS_WORKER_RUNTIME", "python"),
                ("WEBSITE_RESOURCE_GROUP", "my-rg"),
                ("WEBSITE_SITE_NAME", "my-func-app"),
            ]),
            FakeEnv::new(&[
                ("FUNCTIONS_WORKER_RUNTIME", "python"),
                ("WEBSITE_OWNER_NAME", "abc123+flex-host"),
                ("WEBSITE_SITE_NAME", "my-flex-func"),
                ("WEBSITE_SKU", "FlexConsumption"),
            ]),
        ];

        for environment in incomplete_environments {
            assert!(collect_from(environment).is_none());
        }
    }

    #[test]
    fn appends_non_production_slot() {
        let data = collect_from(FakeEnv::new(&[
            ("FUNCTIONS_WORKER_RUNTIME", "python"),
            ("WEBSITE_OWNER_NAME", "abc123+my-rg-eastuswebspace"),
            ("WEBSITE_RESOURCE_GROUP", "my-rg"),
            ("WEBSITE_SITE_NAME", "my-func-app"),
            ("WEBSITE_SLOT_NAME", "Staging"),
        ]))
        .expect("slot identity should be available");
        assert!(data.resource_id.ends_with("/slots/staging"));
    }

    #[test]
    fn omits_production_slot() {
        let data = collect_from(FakeEnv::new(&[
            ("FUNCTIONS_WORKER_RUNTIME", "python"),
            ("WEBSITE_OWNER_NAME", "abc123+my-rg-eastuswebspace"),
            ("WEBSITE_RESOURCE_GROUP", "my-rg"),
            ("WEBSITE_SITE_NAME", "my-func-app"),
            ("WEBSITE_SLOT_NAME", "Production"),
        ]))
        .expect("production identity should be available");
        assert!(!data.resource_id.contains("/slots/"));
    }

    #[test]
    fn enriches_platform_fields() {
        let data = collect_from(FakeEnv::new(&[
            ("FUNCTIONS_WORKER_RUNTIME", "dotnet-isolated"),
            ("FUNCTIONS_WORKER_RUNTIME_VERSION", "8.0"),
            ("REGION_NAME", "East US 2"),
            ("WEBSITE_OWNER_NAME", "abc123+my-rg-eastuswebspace"),
            ("WEBSITE_RESOURCE_GROUP", "my-rg"),
            ("WEBSITE_SITE_NAME", "my-func-app"),
        ]))
        .expect("Azure metadata should be available");
        assert_eq!(data.metadata["runtime"], "dotnet-isolated");
        assert_eq!(data.metadata["serverless_compat_runtime_version"], "8.0");
        assert_eq!(data.metadata["region"], "East US 2");
        assert_eq!(data.metadata["azure_subscription_id"], "abc123");
        assert_eq!(data.metadata["azure_resource_group"], "my-rg");
    }
}
