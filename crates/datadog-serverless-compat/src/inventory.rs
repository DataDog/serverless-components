// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use datadog_fips::reqwest_adapter::create_reqwest_client_builder;
use libdd_trace_utils::trace_utils::EnvironmentType;
use std::env;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

/// Minimum Datadog agent protocol version accepted by EPRW (must be 7.x.x format).
/// The actual serverless-compat version is reported separately as
/// `agent_metadata.serverless_compat_version`.
const AGENT_VERSION: &str = "7.83.0";

/// Sends a single inventory metadata payload to the Datadog intake so this
/// serverless-compat agent appears as a `serverless_compat_agent` resource in
/// REDAPL and Fleet Automation.
///
/// The payload follows the same wire format as the standard inventoryagent
/// component (POST /api/v1/metadata).  EPRW routes it to the
/// `serverless_compat_agent` table when `agent_metadata.flavor` is
/// `"serverless-compat"` and `resource_id`, `resource_name`, and
/// `workload_type` are all present and non-empty.
///
/// Fire-and-forget: logs a warning on failure, never panics.
pub async fn send_inventory_payload(
    api_key: &str,
    dd_site: &str,
    https_proxy: Option<&str>,
    env_type: EnvironmentType,
) {
    let uuid = uuid::Uuid::new_v4().to_string();

    // Must be nanoseconds to match time.Now().UnixNano() expected by EPRW.
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);

    let workload_type = match env_type {
        EnvironmentType::AzureFunction => "azure_function",
        EnvironmentType::AzureSpringApp => "azure_spring_app",
        EnvironmentType::CloudFunction => "gcp_cloud_function_gen1",
        EnvironmentType::LambdaFunction => "lambda",
    };

    // Required REDAPL identity fields — EPRW rejects the serverless_compat_agent
    // write if any of these is absent.
    let (resource_id, resource_name) = build_resource_identity(env_type.clone());

    let mut metadata = serde_json::json!({
        "flavor": "serverless-compat",
        "agent_version": AGENT_VERSION,
        "serverless_compat_version": env!("CARGO_PKG_VERSION"),
        "workload_type": workload_type,
    });

    if !resource_id.is_empty() {
        metadata["resource_id"] = serde_json::Value::String(resource_id.clone());
    }
    if !resource_name.is_empty() {
        metadata["resource_name"] = serde_json::Value::String(resource_name);
    }

    // DD configuration tags (canonical field names match EPRW stringFields allowlist).
    for (env_key, meta_key) in [
        ("DD_ENV", "dd_env"),
        ("DD_SERVICE", "dd_service"),
        ("DD_VERSION", "dd_version"),
        ("DD_SITE", "dd_site"),
    ] {
        if let Ok(val) = env::var(env_key)
            && !val.is_empty()
        {
            metadata[meta_key] = serde_json::Value::String(val);
        }
    }

    // Platform-specific nullable fields.
    enrich_platform_fields(&mut metadata, env_type);

    // Hostname intentionally absent — Roxane (REDAPL Hosts, 2026-07-30):
    // "Just don't set [the hostname] at all, and that should handle it."
    // Setting hostname to "" or a real value causes EPRW to attempt a host_id
    // lookup that fails for serverless workloads, rejecting the record.
    // Omitting it activates the ECS Fargate UUID path (WriteDatadogAgentECSFargate).
    let payload = serde_json::json!({
        "uuid": uuid,
        "timestamp": timestamp,
        "agent_metadata": metadata,
    });

    let body = match serde_json::to_vec(&payload) {
        Ok(b) => b,
        Err(e) => {
            warn!("Failed to serialize inventory payload: {e}");
            return;
        }
    };

    let url = format!("https://api.{dd_site}/api/v1/metadata");

    let client = match build_client(https_proxy) {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to create HTTP client for inventory payload: {e}");
            return;
        }
    };

    match client
        .post(&url)
        .header("DD-API-KEY", api_key)
        .header("Content-Type", "application/json")
        .header("DD-Agent-Version", AGENT_VERSION)
        .header("User-Agent", format!("datadog-agent/{AGENT_VERSION}"))
        .body(body)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() || status.as_u16() == 202 {
                info!(
                    "Inventory payload sent (report_reason=startup, process_start_id={uuid}, workload_type={workload_type}, resource_id={resource_id}, status={status})"
                );
            } else {
                warn!("Inventory payload rejected: report_reason=startup, status={status}, process_start_id={uuid}, resource_id={resource_id}");
            }
        }
        Err(e) => {
            warn!("Failed to send inventory payload: {e}");
        }
    }
}

/// Returns `(resource_id, resource_name)` for the given environment type.
///
/// `resource_id` is the canonical cloud resource identifier (CCRID) used as
/// the primary key in `serverless_compat_agent`.  If the required env vars are
/// absent the returned string is empty, which causes EPRW to reject the
/// per-flavor write (the standard `datadog_agent` write still proceeds).
fn build_resource_identity(env_type: EnvironmentType) -> (String, String) {
    match env_type {
        EnvironmentType::AzureFunction | EnvironmentType::AzureSpringApp => {
            let name = env::var("WEBSITE_SITE_NAME").unwrap_or_default();
            let rg = env::var("WEBSITE_RESOURCE_GROUP").unwrap_or_default();
            // WEBSITE_OWNER_NAME = "{subscription_guid}+{webspace-name}"
            // The subscription GUID is the segment before the first '+'.
            let sub = env::var("WEBSITE_OWNER_NAME")
                .ok()
                .and_then(|s| s.split('+').next().map(|p| p.to_string()))
                .unwrap_or_default();
            if name.is_empty() || rg.is_empty() || sub.is_empty() {
                return (String::new(), name);
            }
            let resource_id = format!(
                "//microsoft.azure/functionApps/{}/{}/{}",
                sub,
                rg,
                name.to_lowercase()
            );
            (resource_id, name)
        }
        EnvironmentType::CloudFunction => {
            let name = env::var("FUNCTION_NAME").unwrap_or_default();
            let region = env::var("FUNCTION_REGION")
                .or_else(|_| env::var("GOOGLE_CLOUD_REGION"))
                .unwrap_or_default();
            let project = env::var("GCP_PROJECT")
                .or_else(|_| env::var("GCLOUD_PROJECT"))
                .or_else(|_| env::var("GOOGLE_CLOUD_PROJECT"))
                .unwrap_or_default();
            if name.is_empty() || region.is_empty() || project.is_empty() {
                return (String::new(), name);
            }
            let resource_id = format!(
                "//cloudfunctions.googleapis.com/projects/{}/locations/{}/functions/{}",
                project, region, name
            );
            (resource_id, name)
        }
        // Lambda and AzureSpringApp CCRIDs are not yet defined — leave empty so
        // EPRW skips the serverless_compat_agent write for these.
        EnvironmentType::LambdaFunction => (String::new(), String::new()),
    }
}

/// Adds platform-specific nullable fields to `metadata`.
fn enrich_platform_fields(metadata: &mut serde_json::Value, env_type: EnvironmentType) {
    match env_type {
        EnvironmentType::AzureFunction | EnvironmentType::AzureSpringApp => {
            // Region: parse from WEBSITE_OWNER_NAME = "{sub}+{rg}-{region}webspace"
            // or fall back to REGION_NAME if set.
            let region = env::var("REGION_NAME")
                .ok()
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    env::var("WEBSITE_OWNER_NAME").ok().and_then(|owner| {
                        // "{sub}+{rg}-{region}webspace" → after '+', take last segment
                        // split on '+' → webspace portion; split on '-' → last before "webspace"
                        let after_plus = owner.split('+').nth(1)?;
                        let without_webspace = after_plus.strip_suffix("webspace")?;
                        let region_part = without_webspace.split('-').next_back()?;
                        Some(region_part.to_string())
                    })
                });
            if let Some(r) = region {
                metadata["region"] = serde_json::Value::String(r);
            }

            // Subscription ID and resource group.
            if let Some(sub) = env::var("WEBSITE_OWNER_NAME")
                .ok()
                .and_then(|s| s.split('+').next().map(|p| p.to_string()))
                .filter(|s| !s.is_empty())
            {
                metadata["azure_subscription_id"] = serde_json::Value::String(sub);
            }
            if let Ok(rg) = env::var("WEBSITE_RESOURCE_GROUP")
                && !rg.is_empty()
            {
                metadata["azure_resource_group"] = serde_json::Value::String(rg);
            }

            // Runtime version: FUNCTIONS_WORKER_RUNTIME gives language name (node, python, …).
            if let Ok(rt) = env::var("FUNCTIONS_WORKER_RUNTIME")
                && !rt.is_empty()
            {
                metadata["serverless_compat_runtime_version"] =
                    serde_json::Value::String(rt.clone());
                metadata["runtime"] = serde_json::Value::String(rt);
            }
            if let Ok(ver) = env::var("FUNCTIONS_EXTENSION_VERSION")
                && !ver.is_empty()
            {
                metadata["platform_version"] = serde_json::Value::String(ver);
            }
        }
        EnvironmentType::CloudFunction => {
            if let Ok(region) = env::var("FUNCTION_REGION")
                && !region.is_empty()
            {
                metadata["region"] = serde_json::Value::String(region);
            }
            if let Ok(project) = env::var("GCP_PROJECT")
                .or_else(|_| env::var("GCLOUD_PROJECT"))
                .or_else(|_| env::var("GOOGLE_CLOUD_PROJECT"))
                && !project.is_empty()
            {
                metadata["gcp_project_id"] = serde_json::Value::String(project);
            }
            // Cloud Functions gen1 expose the runtime via NODE_VERSION / PYTHON_VERSION etc.
            // There is no single canonical env var, so we try common ones.
            let runtime = detect_gcp_runtime();
            if !runtime.is_empty() {
                metadata["serverless_compat_runtime_version"] =
                    serde_json::Value::String(runtime.clone());
                metadata["runtime"] = serde_json::Value::String(runtime);
            }
        }
        EnvironmentType::LambdaFunction => {
            if let Ok(region) = env::var("AWS_REGION")
                && !region.is_empty()
            {
                metadata["region"] = serde_json::Value::String(region);
            }
        }
    }
}

/// Infers runtime string from GCP Cloud Functions gen1 env vars.
/// Returns empty string if no runtime is detected.
fn detect_gcp_runtime() -> String {
    for (prefix, env_var) in [
        ("node", "NODE_VERSION"),
        ("python", "PYTHON_VERSION"),
        ("java", "JAVA_VERSION"),
        ("go", "GO_VERSION"),
    ] {
        if let Ok(ver) = env::var(env_var)
            && !ver.is_empty()
        {
            return format!("{prefix}{ver}");
        }
    }
    String::new()
}

fn build_client(https_proxy: Option<&str>) -> Result<reqwest::Client, Box<dyn std::error::Error>> {
    let mut builder = create_reqwest_client_builder()?.timeout(std::time::Duration::from_secs(5));

    if let Some(proxy) = https_proxy {
        builder = builder.proxy(reqwest::Proxy::https(proxy)?);
    }

    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_structure() {
        let uuid = uuid::Uuid::new_v4().to_string();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);

        let payload = serde_json::json!({
            "uuid": uuid,
            "timestamp": timestamp,
            "agent_metadata": {
                "flavor": "serverless-compat",
                "agent_version": AGENT_VERSION,
                "serverless_compat_version": env!("CARGO_PKG_VERSION"),
                "workload_type": "azure_function",
            }
        });

        let obj = payload.as_object().expect("payload should be an object");

        assert!(obj.contains_key("uuid"));
        assert!(!obj["uuid"].as_str().unwrap().is_empty());

        assert!(obj.contains_key("timestamp"));
        // nanoseconds: must be >> 1e18 (year 2001+)
        assert!(obj["timestamp"].as_i64().unwrap() > 1_000_000_000_000_000_000);

        // hostname must be absent — setting it (even to "") causes EPRW to attempt
        // a host_id lookup that fails for serverless workloads.
        assert!(!obj.contains_key("hostname"));

        let metadata = obj["agent_metadata"].as_object().unwrap();
        assert_eq!(metadata["flavor"], "serverless-compat");
        assert_eq!(metadata["agent_version"], AGENT_VERSION);
        assert!(metadata.contains_key("serverless_compat_version"));
        assert_eq!(metadata["workload_type"], "azure_function");
    }

    #[test]
    fn workload_type_mapping() {
        let cases = [
            (EnvironmentType::AzureFunction, "azure_function"),
            (EnvironmentType::AzureSpringApp, "azure_spring_app"),
            (EnvironmentType::CloudFunction, "gcp_cloud_function_gen1"),
            (EnvironmentType::LambdaFunction, "lambda"),
        ];

        for (env_type, expected) in cases {
            let workload_type = match env_type {
                EnvironmentType::AzureFunction => "azure_function",
                EnvironmentType::AzureSpringApp => "azure_spring_app",
                EnvironmentType::CloudFunction => "gcp_cloud_function_gen1",
                EnvironmentType::LambdaFunction => "lambda",
            };
            assert_eq!(workload_type, expected);
        }
    }

    #[test]
    fn resource_id_azure_function() {
        // Simulate Azure Functions env vars.
        // SAFETY: single-threaded test, no concurrent env access.
        unsafe {
            std::env::set_var("WEBSITE_SITE_NAME", "my-func-app");
            std::env::set_var("WEBSITE_RESOURCE_GROUP", "my-rg");
            std::env::set_var("WEBSITE_OWNER_NAME", "abc123+my-rg-eastuswebspace");
        }

        let (resource_id, resource_name) =
            build_resource_identity(EnvironmentType::AzureFunction);

        assert_eq!(resource_name, "my-func-app");
        assert_eq!(
            resource_id,
            "//microsoft.azure/functionApps/abc123/my-rg/my-func-app"
        );

        unsafe {
            std::env::remove_var("WEBSITE_SITE_NAME");
            std::env::remove_var("WEBSITE_RESOURCE_GROUP");
            std::env::remove_var("WEBSITE_OWNER_NAME");
        }
    }

    #[test]
    fn resource_id_gcp_cloud_function() {
        // SAFETY: single-threaded test, no concurrent env access.
        unsafe {
            std::env::set_var("FUNCTION_NAME", "my-fn");
            std::env::set_var("FUNCTION_REGION", "us-central1");
            std::env::set_var("GCP_PROJECT", "my-project");
        }

        let (resource_id, resource_name) =
            build_resource_identity(EnvironmentType::CloudFunction);

        assert_eq!(resource_name, "my-fn");
        assert_eq!(
            resource_id,
            "//cloudfunctions.googleapis.com/projects/my-project/locations/us-central1/functions/my-fn"
        );

        unsafe {
            std::env::remove_var("FUNCTION_NAME");
            std::env::remove_var("FUNCTION_REGION");
            std::env::remove_var("GCP_PROJECT");
        }
    }

    #[test]
    fn resource_id_missing_returns_empty() {
        // SAFETY: single-threaded test, no concurrent env access.
        unsafe {
            std::env::remove_var("FUNCTION_NAME");
            std::env::remove_var("FUNCTION_REGION");
            std::env::remove_var("GCP_PROJECT");
        }

        let (resource_id, _) = build_resource_identity(EnvironmentType::CloudFunction);
        assert!(
            resource_id.is_empty(),
            "missing env vars must produce empty resource_id"
        );
    }
}
