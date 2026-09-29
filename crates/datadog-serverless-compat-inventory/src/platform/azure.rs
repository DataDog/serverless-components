// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use super::PlatformData;
use libdd_common::azure_app_services::{AzureMetadata, QueryEnv, UNKNOWN_VALUE};
use serde_json::{Map, Value};
use std::env;

struct ProcessEnv;

impl QueryEnv for ProcessEnv {
    fn get_var(&self, name: &str) -> Option<String> {
        env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }
}

fn known_value(value: &str) -> Option<&str> {
    (value != UNKNOWN_VALUE && !value.is_empty()).then_some(value)
}

pub(super) fn collect() -> Option<PlatformData> {
    let azure = AzureMetadata::new_function(ProcessEnv)?;
    let resource_name = known_value(azure.get_site_name())?.to_string();
    let base_resource_id = known_value(azure.get_resource_id())?;

    // Non-production deployment slots share WEBSITE_SITE_NAME with the parent
    // app but have their own ARM path and therefore need a distinct inventory ID.
    let slot = env::var("WEBSITE_SLOT_NAME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("production"));
    let resource_id = match slot {
        Some(slot) => format!("{base_resource_id}/slots/{}", slot.to_lowercase()),
        None => base_resource_id.to_string(),
    };

    let mut metadata = Map::new();
    if let Some(region) = env::var("REGION_NAME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        metadata.insert("region".into(), Value::String(region));
    }

    for (field, value) in [
        ("azure_subscription_id", azure.get_subscription_id()),
        ("azure_resource_group", azure.get_resource_group()),
        // Preserve Azure's documented values instead of inventing a second
        // runtime taxonomy.
        ("runtime", azure.get_runtime()),
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

    static ENV_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

    unsafe fn clear_env() {
        for name in [
            "DD_AZURE_RESOURCE_GROUP",
            "FUNCTIONS_EXTENSION_VERSION",
            "FUNCTIONS_WORKER_RUNTIME",
            "FUNCTIONS_WORKER_RUNTIME_VERSION",
            "REGION_NAME",
            "WEBSITE_OWNER_NAME",
            "WEBSITE_RESOURCE_GROUP",
            "WEBSITE_SITE_NAME",
            "WEBSITE_SKU",
            "WEBSITE_SLOT_NAME",
        ] {
            unsafe { env::remove_var(name) };
        }
    }

    unsafe fn set_base_env() {
        unsafe {
            env::set_var("FUNCTIONS_WORKER_RUNTIME", "python");
            env::set_var("WEBSITE_OWNER_NAME", "abc123+my-rg-eastuswebspace");
            env::set_var("WEBSITE_RESOURCE_GROUP", "my-rg");
            env::set_var("WEBSITE_SITE_NAME", "my-func-app");
        }
    }

    #[test]
    fn builds_full_identity() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            clear_env();
            set_base_env();
        }

        let data = collect().expect("Azure identity should be available");
        assert_eq!(data.resource_name, "my-func-app");
        assert_eq!(
            data.resource_id,
            "/subscriptions/abc123/resourcegroups/my-rg/providers/microsoft.web/sites/my-func-app"
        );

        unsafe { clear_env() };
    }

    #[test]
    fn uses_flex_consumption_resource_group() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            clear_env();
            env::set_var("FUNCTIONS_WORKER_RUNTIME", "python");
            env::set_var("WEBSITE_OWNER_NAME", "abc123+flex-host");
            env::set_var("WEBSITE_SITE_NAME", "my-flex-func");
            env::set_var("WEBSITE_SKU", "FlexConsumption");
            env::set_var("DD_AZURE_RESOURCE_GROUP", "My-Flex-RG");
        }

        let data = collect().expect("Flex identity should be available");
        assert!(data.resource_id.contains("/my-flex-rg/"));

        unsafe { clear_env() };
    }

    #[test]
    fn missing_name_skips_inventory() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            clear_env();
            env::set_var("FUNCTIONS_WORKER_RUNTIME", "python");
            env::set_var("WEBSITE_OWNER_NAME", "abc123+my-rg-eastuswebspace");
            env::set_var("WEBSITE_RESOURCE_GROUP", "my-rg");
        }

        assert!(collect().is_none());
        unsafe { clear_env() };
    }

    #[test]
    fn appends_non_production_slot() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            clear_env();
            set_base_env();
            env::set_var("WEBSITE_SLOT_NAME", "Staging");
        }

        let data = collect().expect("slot identity should be available");
        assert!(data.resource_id.ends_with("/slots/staging"));

        unsafe { clear_env() };
    }

    #[test]
    fn omits_production_slot() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            clear_env();
            set_base_env();
            env::set_var("WEBSITE_SLOT_NAME", "Production");
        }

        let data = collect().expect("production identity should be available");
        assert!(!data.resource_id.contains("/slots/"));

        unsafe { clear_env() };
    }

    #[test]
    fn enriches_platform_fields() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            clear_env();
            set_base_env();
            env::set_var("FUNCTIONS_WORKER_RUNTIME", "dotnet-isolated");
            env::set_var("FUNCTIONS_WORKER_RUNTIME_VERSION", "8.0");
            env::set_var("REGION_NAME", "East US 2");
        }

        let data = collect().expect("Azure metadata should be available");
        assert_eq!(data.metadata["runtime"], "dotnet-isolated");
        assert_eq!(data.metadata["serverless_compat_runtime_version"], "8.0");
        assert_eq!(data.metadata["region"], "East US 2");
        assert_eq!(data.metadata["azure_subscription_id"], "abc123");
        assert_eq!(data.metadata["azure_resource_group"], "my-rg");

        unsafe { clear_env() };
    }
}
