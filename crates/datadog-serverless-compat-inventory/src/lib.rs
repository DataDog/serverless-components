// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

mod payload;
mod platform;

use libdd_trace_utils::trace_utils::EnvironmentType;

/// A serialized inventory report and the identity used for operational logs.
pub struct InventoryReport {
    pub body: Vec<u8>,
    pub resource_id: String,
    pub workload_type: &'static str,
}

/// Builds an inventory report for a supported serverless environment.
///
/// Returns `None` when the environment is unsupported or its required cloud
/// identity is unavailable. The async API leaves room for platforms such as
/// GCP that may need metadata-server lookups to complete their identity.
pub async fn build_inventory_report(
    env_type: &EnvironmentType,
    process_id: &str,
    report_reason: &str,
) -> Result<Option<InventoryReport>, serde_json::Error> {
    let Some(platform) = platform::collect(env_type).await else {
        return Ok(None);
    };

    let body = payload::build(process_id, report_reason, &platform)?;
    Ok(Some(InventoryReport {
        body,
        resource_id: platform.resource_id,
        workload_type: platform.workload_type,
    }))
}
