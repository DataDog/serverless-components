// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use crate::platform::PlatformData;
use serde_json::Value;
use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn build(
    process_id: &str,
    report_reason: &str,
    platform: &PlatformData,
) -> Result<Vec<u8>, serde_json::Error> {
    // EPRW expects nanoseconds, matching time.Now().UnixNano().
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as i64)
        .unwrap_or(0);

    let compat_version = env::var("DD_SERVERLESS_COMPAT_VERSION")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    let mut metadata = serde_json::json!({
        "flavor": "serverless-compat",
        "workload_type": platform.workload_type,
        "report_reason": report_reason,
        "resource_id": platform.resource_id,
        "resource_name": platform.resource_name,
        "serverless_compat_version": compat_version,
    });

    for (key, value) in &platform.metadata {
        metadata[key] = value.clone();
    }

    for (env_key, metadata_key) in [
        ("DD_ENV", "dd_env"),
        ("DD_SERVICE", "dd_service"),
        ("DD_VERSION", "dd_version"),
        ("DD_SITE", "dd_site"),
        ("DD_SERVERLESS_COMPAT_RUNTIME", "runtime"),
        (
            "DD_SERVERLESS_COMPAT_RUNTIME_VERSION",
            "serverless_compat_runtime_version",
        ),
    ] {
        if let Ok(value) = env::var(env_key)
            && !value.is_empty()
        {
            metadata[metadata_key] = Value::String(value);
        }
    }

    // Hostname is intentionally absent: setting it causes EPRW to attempt a
    // host_id lookup that is invalid for serverless workloads.
    serde_json::to_vec(&serde_json::json!({
        "uuid": process_id,
        "timestamp": timestamp,
        "agent_metadata": metadata,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, Value};

    fn azure_platform() -> PlatformData {
        let mut metadata = Map::new();
        metadata.insert("region".into(), Value::String("eastus".into()));
        PlatformData {
            workload_type: "azure_function",
            resource_id: "/subscriptions/sub/resourcegroups/rg/providers/microsoft.web/sites/app"
                .into(),
            resource_name: "app".into(),
            metadata,
        }
    }

    #[test]
    fn builds_azure_payload_shape() {
        let body = build("process-id", "startup", &azure_platform()).unwrap();
        let payload: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(payload["uuid"], "process-id");
        assert!(payload["timestamp"].as_i64().unwrap() > 0);
        assert_eq!(payload["agent_metadata"]["flavor"], "serverless-compat");
        assert_eq!(payload["agent_metadata"]["workload_type"], "azure_function");
        assert_eq!(payload["agent_metadata"]["report_reason"], "startup");
        assert_eq!(payload["agent_metadata"]["region"], "eastus");
        assert!(payload.get("hostname").is_none());
    }
}
