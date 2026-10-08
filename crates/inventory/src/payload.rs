// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use crate::{ProcessEnv, platform::PlatformData};
use libdd_common::azure_app_services::QueryEnv;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn build(
    process_id: &str,
    report_reason: &str,
    platform: &PlatformData,
) -> Result<Vec<u8>, serde_json::Error> {
    build_with_env(
        process_id,
        report_reason,
        platform,
        &ProcessEnv,
        option_env!("DD_SERVERLESS_COMPAT_VERSION"),
    )
}

fn build_with_env(
    process_id: &str,
    report_reason: &str,
    platform: &PlatformData,
    env: &impl QueryEnv,
    embedded_version: Option<&str>,
) -> Result<Vec<u8>, serde_json::Error> {
    // EPRW expects nanoseconds, matching time.Now().UnixNano().
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as i64)
        .unwrap_or(0);

    let compat_version = env
        .get_var("DD_SERVERLESS_COMPAT_VERSION")
        .filter(|value| !value.is_empty());
    let compat_binary_version = embedded_version
        .map(str::trim)
        .filter(|value| !value.is_empty());

    // Required identity fields take precedence over platform-specific metadata.
    let mut metadata = Value::Object(platform.metadata.clone());
    metadata["flavor"] = Value::String("serverless-compat".into());
    metadata["workload_type"] = Value::String(platform.workload_type.into());
    metadata["report_reason"] = Value::String(report_reason.into());
    metadata["resource_id"] = Value::String(platform.resource_id.clone());
    metadata["resource_name"] = Value::String(platform.resource_name.clone());
    if let Some(compat_version) = compat_version {
        metadata["serverless_compat_version"] = Value::String(compat_version);
    }
    if let Some(compat_binary_version) = compat_binary_version {
        metadata["serverless_compat_binary_version"] =
            Value::String(compat_binary_version.to_string());
    }

    for (env_key, metadata_key) in [
        ("DD_ENV", "dd_env"),
        ("DD_SERVICE", "dd_service"),
        ("DD_VERSION", "dd_version"),
        ("DD_SITE", "dd_site"),
    ] {
        if let Some(value) = env.get_var(env_key).filter(|value| !value.is_empty()) {
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
    use crate::test_env::FakeEnv;
    use serde_json::{Map, Value};

    fn azure_platform() -> PlatformData {
        let mut metadata = Map::new();
        metadata.insert("region".into(), Value::String("eastus".into()));
        metadata.insert("runtime".into(), Value::String("python3.13".into()));
        metadata.insert("flavor".into(), Value::String("incorrect".into()));
        metadata.insert("resource_id".into(), Value::String("incorrect".into()));
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
        let body = build_with_env(
            "process-id",
            "startup",
            &azure_platform(),
            &FakeEnv::default(),
            Some("1.2.3"),
        )
        .unwrap();
        let payload: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(payload["uuid"], "process-id");
        assert!(payload["timestamp"].as_i64().unwrap() > 0);
        assert_eq!(payload["agent_metadata"]["flavor"], "serverless-compat");
        assert_eq!(
            payload["agent_metadata"]["resource_id"],
            "/subscriptions/sub/resourcegroups/rg/providers/microsoft.web/sites/app"
        );
        assert_eq!(payload["agent_metadata"]["workload_type"], "azure_function");
        assert_eq!(payload["agent_metadata"]["report_reason"], "startup");
        assert_eq!(payload["agent_metadata"]["region"], "eastus");
        assert_eq!(
            payload["agent_metadata"]["serverless_compat_binary_version"],
            "1.2.3"
        );
        assert!(
            payload["agent_metadata"]
                .get("serverless_compat_version")
                .is_none()
        );
        assert!(payload.get("hostname").is_none());
    }

    #[test]
    fn reports_runtime_compat_and_binary_versions_separately() {
        let env = FakeEnv::new(&[("DD_SERVERLESS_COMPAT_VERSION", "0.18.0")]);
        let body =
            build_with_env("pid", "startup", &azure_platform(), &env, Some("0.28.0")).unwrap();
        let payload: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(
            payload["agent_metadata"]["serverless_compat_version"],
            "0.18.0"
        );
        assert_eq!(
            payload["agent_metadata"]["serverless_compat_binary_version"],
            "0.28.0"
        );
        assert_eq!(payload["agent_metadata"]["runtime"], "python3.13");
    }

    #[test]
    fn omits_unknown_compat_version() {
        let body = build_with_env(
            "pid",
            "startup",
            &azure_platform(),
            &FakeEnv::default(),
            None,
        )
        .unwrap();
        let payload: Value = serde_json::from_slice(&body).unwrap();

        assert!(
            payload["agent_metadata"]
                .get("serverless_compat_version")
                .is_none()
        );
        assert!(
            payload["agent_metadata"]
                .get("serverless_compat_binary_version")
                .is_none()
        );
    }
}
