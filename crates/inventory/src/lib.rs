// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

mod payload;
mod platform;
mod reporter;

pub use reporter::run_inventory_reporter;

use libdd_common::azure_app_services::QueryEnv;
use std::env;

#[derive(Clone, Copy)]
struct ProcessEnv;

impl QueryEnv for ProcessEnv {
    fn get_var(&self, name: &str) -> Option<String> {
        env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }
}

/// A serialized inventory report and the identity used for operational logs.
pub struct InventoryReport {
    pub body: Vec<u8>,
    pub resource_id: String,
    pub workload_type: &'static str,
}

/// Builds an inventory report from the cloud identity collected at startup.
fn build_inventory_report(
    platform: &platform::PlatformData,
    process_id: &str,
    report_reason: &str,
    dd_site: &str,
) -> Result<InventoryReport, serde_json::Error> {
    let body = payload::build(process_id, report_reason, platform, dd_site)?;
    Ok(InventoryReport {
        body,
        resource_id: platform.resource_id.clone(),
        workload_type: platform.workload_type,
    })
}

#[cfg(test)]
mod test_env {
    use libdd_common::azure_app_services::QueryEnv;
    use std::collections::HashMap;

    #[derive(Clone, Default)]
    pub(crate) struct FakeEnv(HashMap<&'static str, &'static str>);

    impl FakeEnv {
        pub(crate) fn new(values: &[(&'static str, &'static str)]) -> Self {
            Self(values.iter().copied().collect())
        }
    }

    impl QueryEnv for FakeEnv {
        fn get_var(&self, name: &str) -> Option<String> {
            self.0.get(name).map(|value| value.to_string())
        }
    }
}
