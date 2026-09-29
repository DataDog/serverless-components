// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

mod azure;
mod gcp;

use libdd_trace_utils::trace_utils::EnvironmentType;
use serde_json::{Map, Value};

#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

pub(crate) struct PlatformData {
    pub workload_type: &'static str,
    pub resource_id: String,
    pub resource_name: String,
    pub metadata: Map<String, Value>,
}

pub(crate) async fn collect(env_type: &EnvironmentType) -> Option<PlatformData> {
    match env_type {
        EnvironmentType::AzureFunction => azure::collect(),
        EnvironmentType::CloudFunction => gcp::collect().await,
        EnvironmentType::LambdaFunction | EnvironmentType::AzureSpringApp => None,
    }
}
