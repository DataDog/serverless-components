// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use datadog_fips::reqwest_adapter::create_reqwest_client_builder;
use libdd_common::azure_app_services::{AzureMetadata, QueryEnv, UNKNOWN_VALUE};
use libdd_trace_utils::trace_utils::EnvironmentType;
use std::env;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::interval;
use tracing::{debug, warn};

/// How often to send a periodic inventory report while the mini-agent is running.
const INVENTORY_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Maximum total send attempts for transient failures (429, 5xx, transport errors).
/// Includes the initial attempt — 3 means one try plus up to two retries.
const MAX_RETRIES: u32 = 3;

/// Supported Compat workload types. Unsupported env types (Lambda, Azure Spring
/// Apps) are silently skipped so they never create a `serverless_compat_agent` row.
fn supported_workload_type(env_type: &EnvironmentType) -> Option<&'static str> {
    match env_type {
        EnvironmentType::AzureFunction => Some("azure_function"),
        EnvironmentType::CloudFunction => Some("cloud_function"),
        // Lambda and Azure Spring Apps are not supported by serverless_compat_agent.
        EnvironmentType::LambdaFunction | EnvironmentType::AzureSpringApp => None,
    }
}

/// Returns true only when `DD_SERVERLESS_COMPAT_INVENTORY_ENABLED=true`.
/// Extracted so the gate logic can be unit-tested without an async runtime.
fn is_inventory_enabled() -> bool {
    env::var("DD_SERVERLESS_COMPAT_INVENTORY_ENABLED").as_deref() == Ok("true")
}

/// Runs the inventory reporter for the lifetime of the mini-agent.
///
/// Sends a startup report immediately, then a periodic report every
/// [`INVENTORY_INTERVAL`]. Spawned as a background task — never panics,
/// never blocks agent startup.
pub async fn run_inventory_reporter(
    api_key: &str,
    dd_site: &str,
    https_proxy: Option<&str>,
    env_type: EnvironmentType,
) {
    if !is_inventory_enabled() {
        // Rollout gate: inventory is opt-in during the ramp. Default is off.
        return;
    }

    let Some(workload_type) = supported_workload_type(&env_type) else {
        // Unsupported workload: skip silently.
        return;
    };

    let client = match build_client(https_proxy) {
        Ok(c) => c,
        Err(e) => {
            warn!("inventory: failed to create HTTP client: {e}");
            return;
        }
    };

    // Stable process UUID — reused across all reports from this process.
    let process_id = uuid::Uuid::new_v4().to_string();

    // Startup report.
    send_report(
        &client,
        api_key,
        dd_site,
        &env_type,
        workload_type,
        &process_id,
        "startup",
    )
    .await;

    // Periodic reports.
    let mut ticker = interval(INVENTORY_INTERVAL);
    ticker.tick().await; // consumes the immediate first tick
    loop {
        ticker.tick().await;
        send_report(
            &client,
            api_key,
            dd_site,
            &env_type,
            workload_type,
            &process_id,
            "periodic",
        )
        .await;
    }
}

/// Builds and sends one inventory report with bounded retry for transient failures.
async fn send_report(
    client: &reqwest::Client,
    api_key: &str,
    dd_site: &str,
    env_type: &EnvironmentType,
    workload_type: &str,
    process_id: &str,
    report_reason: &str,
) {
    // Reuse libdatadog's canonical Azure parsing for identity and enrichment.
    // This includes Flex Consumption's DD_AZURE_RESOURCE_GROUP fallback.
    let azure_metadata = if matches!(env_type, EnvironmentType::AzureFunction) {
        AzureMetadata::new_function(ProcessEnv)
    } else {
        None
    };

    let (mut resource_id, resource_name) =
        build_resource_identity(env_type, azure_metadata.as_ref());

    // Gen1 Cloud Functions: if FUNCTION_NAME was present but region/project were
    // absent from env vars, try the GCP instance metadata server to complete the
    // resource_id. resource_name is non-empty only when a function name was found;
    // if both are empty, this is an unrecognised environment — skip entirely.
    if matches!(env_type, EnvironmentType::CloudFunction)
        && resource_id.is_empty()
        && !resource_name.is_empty()
    {
        let project_from_env = env::var("GCP_PROJECT")
            .or_else(|_| env::var("GCLOUD_PROJECT"))
            .or_else(|_| env::var("GOOGLE_CLOUD_PROJECT"))
            .ok()
            .filter(|s| !s.is_empty());

        let (region_opt, project_opt) = if project_from_env.is_some() {
            (fetch_gcp_region_from_metadata().await, project_from_env)
        } else {
            let (r, p) = tokio::join!(
                fetch_gcp_region_from_metadata(),
                fetch_gcp_project_from_metadata(),
            );
            (r, p)
        };

        if let (Some(region), Some(project)) = (region_opt, project_opt) {
            resource_id = format!(
                "//cloudfunctions.googleapis.com/projects/{}/locations/{}/functions/{}",
                project, region, resource_name
            );
        }
    }

    // EPRW rejects the serverless_compat_agent write when required fields are absent.
    // Log and skip rather than sending a doomed payload.
    if resource_id.is_empty() {
        warn!(
            "inventory: required identity unavailable, skipping report \
             (report_reason={report_reason}, workload_type={workload_type})"
        );
        return;
    }

    let body = match build_payload(
        process_id,
        workload_type,
        report_reason,
        &resource_id,
        &resource_name,
        env_type,
        azure_metadata.as_ref(),
    ) {
        Ok(b) => b,
        Err(e) => {
            warn!("inventory: failed to serialize payload: {e}");
            return;
        }
    };

    let url = format!("https://api.{dd_site}/api/v1/metadata");

    for attempt in 0..MAX_RETRIES {
        match do_send(client, &url, api_key, body.clone()).await {
            Ok(status) if status < 300 => {
                debug!(
                    "inventory: report sent \
                     (report_reason={report_reason}, workload_type={workload_type}, \
                     status={status})"
                );
                return;
            }
            Ok(429) | Ok(500..=599) if attempt + 1 < MAX_RETRIES => {
                let backoff = Duration::from_secs(1 << attempt);
                warn!(
                    "inventory: transient failure, retrying in {backoff:?} \
                     (report_reason={report_reason}, attempt={})",
                    attempt + 1
                );
                tokio::time::sleep(backoff).await;
            }
            Ok(status) => {
                warn!(
                    "inventory: intake rejected report \
                     (report_reason={report_reason}, workload_type={workload_type}, \
                     status={status})"
                );
                return;
            }
            Err(e) if attempt + 1 < MAX_RETRIES => {
                let backoff = Duration::from_secs(1 << attempt);
                warn!(
                    "inventory: transport error, retrying in {backoff:?} \
                     (report_reason={report_reason}, attempt={}, error={e})",
                    attempt + 1
                );
                tokio::time::sleep(backoff).await;
            }
            Err(e) => {
                warn!(
                    "inventory: transport error after {MAX_RETRIES} attempts \
                     (report_reason={report_reason}, error={e})"
                );
                return;
            }
        }
    }
}

/// Sends the raw payload body, returning the HTTP status code or a transport error.
async fn do_send(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    body: Vec<u8>,
) -> Result<u16, reqwest::Error> {
    let resp = client
        .post(url)
        .header("DD-API-KEY", api_key)
        .header("Content-Type", "application/json")
        .header(
            "User-Agent",
            format!("datadog-serverless-compat/{}", env!("CARGO_PKG_VERSION")),
        )
        .body(body)
        .send()
        .await?;
    Ok(resp.status().as_u16())
}

/// Builds the serialized JSON inventory payload.
fn build_payload(
    process_id: &str,
    workload_type: &str,
    report_reason: &str,
    resource_id: &str,
    resource_name: &str,
    env_type: &EnvironmentType,
    azure_metadata: Option<&AzureMetadata>,
) -> Result<Vec<u8>, serde_json::Error> {
    // Must be nanoseconds to match time.Now().UnixNano() expected by EPRW.
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or_else(|e| {
            warn!("inventory: system clock error, timestamp will be 0: {e}");
            0
        });

    // serverless_compat_version: prefer DD_SERVERLESS_COMPAT_VERSION (set by the
    // language package wrapping this binary); fall back to the Rust crate version
    // when running standalone.
    let compat_version = env::var("DD_SERVERLESS_COMPAT_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    let mut metadata = serde_json::json!({
        "flavor": "serverless-compat",
        "workload_type": workload_type,
        "report_reason": report_reason,
        "resource_id": resource_id,
        "resource_name": resource_name,
        "serverless_compat_version": compat_version,
    });

    // DD unified service tags.
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

    // Platform-specific optional fields (runtime, region, cloud IDs, etc.).
    enrich_platform_fields(&mut metadata, env_type, azure_metadata);

    // Hostname intentionally absent: setting it (even to "") causes EPRW to
    // attempt a host_id lookup that fails for serverless workloads, rejecting
    // the record. Omitting it activates the ECS Fargate UUID path in EPRW.
    let payload = serde_json::json!({
        "uuid": process_id,
        "timestamp": timestamp,
        "agent_metadata": metadata,
    });

    serde_json::to_vec(&payload)
}

/// Returns `(resource_id, resource_name)` for supported Compat workloads.
///
/// `resource_id` is the canonical cloud resource identifier used as the primary
/// key in `serverless_compat_agent`. An empty `resource_id` means the required
/// environment variables are absent; the caller must skip the write.
fn build_resource_identity(
    env_type: &EnvironmentType,
    azure_metadata: Option<&AzureMetadata>,
) -> (String, String) {
    match env_type {
        EnvironmentType::AzureFunction => build_azure_function_identity(azure_metadata),
        EnvironmentType::CloudFunction => build_cloud_function_identity(),
        EnvironmentType::LambdaFunction | EnvironmentType::AzureSpringApp => {
            (String::new(), String::new())
        }
    }
}

struct ProcessEnv;

impl QueryEnv for ProcessEnv {
    fn get_var(&self, name: &str) -> Option<String> {
        env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }
}

fn known_azure_value(value: &str) -> Option<&str> {
    (value != UNKNOWN_VALUE && !value.is_empty()).then_some(value)
}

fn build_azure_function_identity(metadata: Option<&AzureMetadata>) -> (String, String) {
    let Some(metadata) = metadata else {
        return (String::new(), String::new());
    };

    let name = known_azure_value(metadata.get_site_name())
        .unwrap_or_default()
        .to_string();
    let Some(base_resource_id) = known_azure_value(metadata.get_resource_id()) else {
        return (String::new(), name);
    };

    // Non-production deployment slots share WEBSITE_SITE_NAME with the parent app but
    // expose a distinct ARM path: /sites/{name}/slots/{slot}. Azure sets WEBSITE_SLOT_NAME
    // for every slot; the production slot uses the value "production". Appending the slot
    // path ensures each slot gets its own inventory row rather than overwriting the parent.
    let slot = env::var("WEBSITE_SLOT_NAME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("production"));

    let resource_id = match slot {
        Some(slot) => format!("{base_resource_id}/slots/{}", slot.to_lowercase()),
        None => base_resource_id.to_string(),
    };
    (resource_id, name)
}

fn build_cloud_function_identity() -> (String, String) {
    // The compat binary is only ever installed in Gen1 Cloud Functions.
    // Gen2 (Cloud Run Functions) uses serverless-init via sidecar instead.
    // We no longer guard on FUNCTION_TARGET here: newer Gen1 runtimes
    // (Python 3.11, Node.js 20+) run on Cloud Run infrastructure and set
    // FUNCTION_TARGET to the entry-point name — indistinguishable from Gen2
    // purely on env vars. Removing the guard is safe because the binary is
    // only present when the user explicitly installed the compat package.

    // Gen1: FUNCTION_NAME is canonical; newer Gen1 runtimes on Cloud Run infra
    // may omit it and expose K_SERVICE instead.
    let name = env::var("FUNCTION_NAME")
        .or_else(|_| env::var("K_SERVICE"))
        .unwrap_or_default();
    if name.is_empty() {
        return (String::new(), String::new());
    }

    let region = env::var("FUNCTION_REGION")
        .or_else(|_| env::var("GOOGLE_CLOUD_REGION"))
        .or_else(|_| env::var("REGION_NAME"))
        .unwrap_or_default();
    let project = env::var("GCP_PROJECT")
        .or_else(|_| env::var("GCLOUD_PROJECT"))
        .or_else(|_| env::var("GOOGLE_CLOUD_PROJECT"))
        .unwrap_or_default();

    if region.is_empty() || project.is_empty() {
        // Region/project absent from env vars — caller may retry via metadata server.
        return (String::new(), name);
    }

    let resource_id = format!(
        "//cloudfunctions.googleapis.com/projects/{}/locations/{}/functions/{}",
        project, region, name
    );
    (resource_id, name)
}

/// Fetches a single field from the GCP instance metadata server.
async fn fetch_gcp_metadata_value(
    path: &str,
    label: &str,
    parse: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let client = match create_reqwest_client_builder().and_then(|builder| {
        builder
            .timeout(Duration::from_secs(2))
            .build()
            .map_err(Into::into)
    }) {
        Ok(client) => client,
        Err(error) => {
            warn!("inventory: failed to create GCP metadata client for {label}: {error}");
            return None;
        }
    };

    let url = format!("http://metadata.google.internal/computeMetadata/v1/{path}");
    let resp = match client
        .get(&url)
        .header("Metadata-Flavor", "Google")
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            warn!("inventory: failed to fetch GCP metadata {label}: {error}");
            return None;
        }
    };

    if !resp.status().is_success() {
        warn!(
            "inventory: GCP metadata server returned {} for {label}",
            resp.status()
        );
        return None;
    }

    let body = match resp.text().await {
        Ok(body) => body,
        Err(error) => {
            warn!("inventory: failed to read GCP metadata {label}: {error}");
            return None;
        }
    };
    let result = parse(body.trim());
    debug!("inventory: GCP metadata server {label}: {:?}", result);
    result
}

async fn fetch_gcp_region_from_metadata() -> Option<String> {
    // Response: "projects/<project-number>/regions/<region-name>"
    fetch_gcp_metadata_value("instance/region", "region", |body| {
        body.split('/')
            .next_back()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
    .await
}

async fn fetch_gcp_project_from_metadata() -> Option<String> {
    fetch_gcp_metadata_value("project/project-id", "project-id", |body| {
        if body.is_empty() {
            None
        } else {
            Some(body.to_string())
        }
    })
    .await
}

/// Adds platform-specific optional fields to `metadata`.
fn enrich_platform_fields(
    metadata: &mut serde_json::Value,
    env_type: &EnvironmentType,
    azure_metadata: Option<&AzureMetadata>,
) {
    match env_type {
        EnvironmentType::AzureFunction => enrich_azure_function_fields(metadata, azure_metadata),
        EnvironmentType::CloudFunction => enrich_cloud_function_fields(metadata),
        EnvironmentType::LambdaFunction | EnvironmentType::AzureSpringApp => {}
    }
}

fn enrich_azure_function_fields(
    metadata: &mut serde_json::Value,
    azure_metadata: Option<&AzureMetadata>,
) {
    // REGION_NAME is the canonical Azure-provided region value.
    let region = env::var("REGION_NAME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let Some(r) = region {
        metadata["region"] = serde_json::Value::String(r);
    }

    if let Some(azure_metadata) = azure_metadata {
        for (field, value) in [
            (
                "azure_subscription_id",
                azure_metadata.get_subscription_id(),
            ),
            ("azure_resource_group", azure_metadata.get_resource_group()),
            // Preserve Azure's documented raw runtime values (for example node,
            // python, dotnet, or dotnet-isolated); do not invent a second taxonomy.
            ("runtime", azure_metadata.get_runtime()),
            (
                "serverless_compat_runtime_version",
                azure_metadata.get_runtime_version(),
            ),
        ] {
            if let Some(value) = known_azure_value(value) {
                metadata[field] = serde_json::Value::String(value.to_string());
            }
        }
    }
}

fn enrich_cloud_function_fields(metadata: &mut serde_json::Value) {
    let region = env::var("FUNCTION_REGION")
        .or_else(|_| env::var("GOOGLE_CLOUD_REGION"))
        .or_else(|_| env::var("REGION_NAME"))
        .ok()
        .filter(|s| !s.is_empty());
    if let Some(r) = region {
        metadata["region"] = serde_json::Value::String(r);
    }

    let project = env::var("GCP_PROJECT")
        .or_else(|_| env::var("GCLOUD_PROJECT"))
        .or_else(|_| env::var("GOOGLE_CLOUD_PROJECT"))
        .ok()
        .filter(|s| !s.is_empty());
    if let Some(p) = project {
        metadata["gcp_project_id"] = serde_json::Value::String(p);
    }

    // These values come from the GCP runtime itself. The language packages set
    // DD_SERVERLESS_COMPAT_VERSION, which is a package version and must not be
    // confused with the language runtime version.
    let (lang, ver) = detect_gcp_gen1_runtime();

    if !lang.is_empty() {
        metadata["runtime"] = serde_json::Value::String(lang);
    }
    if !ver.is_empty() {
        metadata["serverless_compat_runtime_version"] = serde_json::Value::String(ver);
    }
}

/// Infers the runtime language and version for GCP Cloud Functions Gen1 from
/// well-known environment variables injected by the GCP runtime.
fn detect_gcp_gen1_runtime() -> (String, String) {
    for (lang, env_var) in [
        ("node", "NODE_VERSION"),
        ("python", "PYTHON_VERSION"),
        ("java", "JAVA_VERSION"),
        ("go", "GO_VERSION"),
    ] {
        if let Ok(ver) = env::var(env_var)
            && !ver.is_empty()
        {
            return (lang.to_string(), ver);
        }
    }
    (String::new(), String::new())
}

fn build_client(https_proxy: Option<&str>) -> Result<reqwest::Client, Box<dyn std::error::Error>> {
    let mut builder = create_reqwest_client_builder()?.timeout(Duration::from_secs(10));

    if let Some(proxy) = https_proxy {
        builder = builder.proxy(reqwest::Proxy::https(proxy)?);
    }

    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mutex to serialize tests that read or write process-global env vars.
    static ENV_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

    unsafe fn clear_azure_function_env() {
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

    fn current_azure_metadata() -> AzureMetadata {
        AzureMetadata::new_function(ProcessEnv).expect("Azure Functions metadata should be built")
    }

    // ── Workload type filtering ──────────────────────────────────────────────

    #[test]
    fn supported_workloads_accepted() {
        assert!(supported_workload_type(&EnvironmentType::AzureFunction).is_some());
        assert!(supported_workload_type(&EnvironmentType::CloudFunction).is_some());
    }

    #[test]
    fn unsupported_workloads_skipped() {
        assert!(supported_workload_type(&EnvironmentType::LambdaFunction).is_none());
        assert!(supported_workload_type(&EnvironmentType::AzureSpringApp).is_none());
    }

    #[test]
    fn azure_function_workload_type() {
        assert_eq!(
            supported_workload_type(&EnvironmentType::AzureFunction),
            Some("azure_function")
        );
    }

    #[test]
    fn cloud_function_workload_type() {
        assert_eq!(
            supported_workload_type(&EnvironmentType::CloudFunction),
            Some("cloud_function")
        );
    }

    // ── Azure Function identity ──────────────────────────────────────────────

    #[test]
    fn azure_function_identity_full() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            clear_azure_function_env();
            env::set_var("FUNCTIONS_WORKER_RUNTIME", "python");
            env::set_var("WEBSITE_OWNER_NAME", "abc123+my-rg-eastuswebspace");
            env::set_var("WEBSITE_SITE_NAME", "my-func-app");
            env::set_var("WEBSITE_RESOURCE_GROUP", "my-rg");
        }

        let azure_metadata = current_azure_metadata();
        let (id, name) = build_azure_function_identity(Some(&azure_metadata));

        assert_eq!(name, "my-func-app");
        assert_eq!(
            id,
            "/subscriptions/abc123/resourcegroups/my-rg/providers/microsoft.web/sites/my-func-app"
        );

        unsafe {
            clear_azure_function_env();
        }
    }

    #[test]
    fn azure_function_identity_rg_from_owner_name() {
        // WEBSITE_RESOURCE_GROUP absent; RG parsed from WEBSITE_OWNER_NAME.
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            clear_azure_function_env();
            env::set_var("FUNCTIONS_WORKER_RUNTIME", "python");
            env::set_var(
                "WEBSITE_OWNER_NAME",
                "sub123+my-resource-group-westus2webspace-Linux",
            );
            env::set_var("WEBSITE_SITE_NAME", "my-func");
        }

        let azure_metadata = current_azure_metadata();
        let (id, name) = build_azure_function_identity(Some(&azure_metadata));

        assert_eq!(name, "my-func");
        assert!(
            id.contains("/my-resource-group/"),
            "expected RG in id: {id}"
        );

        unsafe {
            clear_azure_function_env();
        }
    }

    #[test]
    fn azure_function_identity_flex_consumption_uses_dd_resource_group() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            clear_azure_function_env();
            env::set_var("FUNCTIONS_WORKER_RUNTIME", "python");
            env::set_var("WEBSITE_OWNER_NAME", "abc123+flex-host");
            env::set_var("WEBSITE_SITE_NAME", "my-flex-func");
            env::set_var("WEBSITE_SKU", "FlexConsumption");
            env::set_var("DD_AZURE_RESOURCE_GROUP", "My-Flex-RG");
        }

        let azure_metadata = current_azure_metadata();
        let (id, name) = build_azure_function_identity(Some(&azure_metadata));

        assert_eq!(name, "my-flex-func");
        assert_eq!(
            id,
            "/subscriptions/abc123/resourcegroups/my-flex-rg/providers/microsoft.web/sites/my-flex-func"
        );

        unsafe {
            clear_azure_function_env();
        }
    }

    #[test]
    fn azure_function_identity_missing_name_returns_empty() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            clear_azure_function_env();
            env::set_var("FUNCTIONS_WORKER_RUNTIME", "python");
            env::set_var("WEBSITE_OWNER_NAME", "abc123+my-rg-eastuswebspace");
            env::set_var("WEBSITE_RESOURCE_GROUP", "my-rg");
        }

        let azure_metadata = current_azure_metadata();
        let (id, _name) = build_azure_function_identity(Some(&azure_metadata));
        assert!(
            id.is_empty(),
            "missing WEBSITE_SITE_NAME must produce empty resource_id"
        );

        unsafe {
            clear_azure_function_env();
        }
    }

    #[test]
    fn azure_function_identity_non_production_slot() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            clear_azure_function_env();
            env::set_var("FUNCTIONS_WORKER_RUNTIME", "python");
            env::set_var("WEBSITE_OWNER_NAME", "abc123+my-rg-eastuswebspace");
            env::set_var("WEBSITE_SITE_NAME", "my-func-app");
            env::set_var("WEBSITE_RESOURCE_GROUP", "my-rg");
            env::set_var("WEBSITE_SLOT_NAME", "staging");
        }

        let azure_metadata = current_azure_metadata();
        let (id, name) = build_azure_function_identity(Some(&azure_metadata));

        assert_eq!(name, "my-func-app");
        assert!(
            id.ends_with("/slots/staging"),
            "non-production slot must appear in resource_id; got: {id}"
        );

        unsafe {
            clear_azure_function_env();
        }
    }

    #[test]
    fn azure_function_identity_production_slot_omitted() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            clear_azure_function_env();
            env::set_var("FUNCTIONS_WORKER_RUNTIME", "python");
            env::set_var("WEBSITE_OWNER_NAME", "abc123+my-rg-eastuswebspace");
            env::set_var("WEBSITE_SITE_NAME", "my-func-app");
            env::set_var("WEBSITE_RESOURCE_GROUP", "my-rg");
            env::set_var("WEBSITE_SLOT_NAME", "Production");
        }

        let azure_metadata = current_azure_metadata();
        let (id, _name) = build_azure_function_identity(Some(&azure_metadata));

        assert!(
            !id.contains("/slots/"),
            "production slot must not appear in resource_id; got: {id}"
        );

        unsafe {
            clear_azure_function_env();
        }
    }

    #[test]
    fn azure_function_enrichment_uses_platform_runtime_values() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            clear_azure_function_env();
            env::set_var("FUNCTIONS_WORKER_RUNTIME", "dotnet-isolated");
            env::set_var("FUNCTIONS_WORKER_RUNTIME_VERSION", "8.0");
            env::set_var("REGION_NAME", "East US 2");
            env::set_var("WEBSITE_OWNER_NAME", "abc123+my-rg-eastus2webspace");
            env::set_var("WEBSITE_RESOURCE_GROUP", "my-rg");
            env::set_var("WEBSITE_SITE_NAME", "my-func-app");
        }

        let azure_metadata = current_azure_metadata();
        let mut metadata = serde_json::json!({});
        enrich_azure_function_fields(&mut metadata, Some(&azure_metadata));

        assert_eq!(metadata["runtime"], "dotnet-isolated");
        assert_eq!(metadata["serverless_compat_runtime_version"], "8.0");
        assert_eq!(metadata["region"], "East US 2");
        assert_eq!(metadata["azure_subscription_id"], "abc123");
        assert_eq!(metadata["azure_resource_group"], "my-rg");

        unsafe {
            clear_azure_function_env();
        }
    }

    // ── GCP Cloud Function identity ──────────────────────────────────────────

    #[test]
    fn cloud_function_gen1_identity_full() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            env::remove_var("FUNCTION_TARGET");
            env::set_var("FUNCTION_NAME", "my-fn");
            env::set_var("FUNCTION_REGION", "us-central1");
            env::set_var("GCP_PROJECT", "my-project");
        }

        let (id, name) = build_cloud_function_identity();

        assert_eq!(name, "my-fn");
        assert_eq!(
            id,
            "//cloudfunctions.googleapis.com/projects/my-project/locations/us-central1/functions/my-fn"
        );

        unsafe {
            env::remove_var("FUNCTION_NAME");
            env::remove_var("FUNCTION_REGION");
            env::remove_var("GCP_PROJECT");
        }
    }

    #[test]
    fn cloud_function_gen1_region_name_fallback() {
        // Gen1 on Cloud Run infra: FUNCTION_REGION absent, REGION_NAME present.
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            env::remove_var("FUNCTION_TARGET");
            env::set_var("FUNCTION_NAME", "my-fn");
            env::remove_var("FUNCTION_REGION");
            env::remove_var("GOOGLE_CLOUD_REGION");
            env::set_var("REGION_NAME", "us-central1");
            env::set_var("GCP_PROJECT", "my-project");
        }

        let (id, name) = build_cloud_function_identity();

        assert_eq!(name, "my-fn");
        assert!(id.contains("us-central1"), "expected region in id: {id}");

        unsafe {
            env::remove_var("FUNCTION_NAME");
            env::remove_var("REGION_NAME");
            env::remove_var("GCP_PROJECT");
        }
    }

    #[test]
    fn cloud_function_newer_gen1_with_function_target_still_reported() {
        // Newer Gen1 runtimes (Python 3.11+, Node.js 20+) run on Cloud Run
        // infrastructure and set FUNCTION_TARGET alongside K_SERVICE. The compat
        // binary's presence is the gate — we always report if installed.
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            env::set_var("K_SERVICE", "my-service");
            env::set_var("FUNCTION_TARGET", "my-handler");
            env::set_var("REGION_NAME", "us-central1");
            env::set_var("GCP_PROJECT", "my-project");
        }

        let (id, name) = build_cloud_function_identity();

        assert_eq!(name, "my-service");
        assert!(
            id.contains("my-service"),
            "newer Gen1 with FUNCTION_TARGET must still produce resource_id; got: {id}"
        );

        unsafe {
            env::remove_var("K_SERVICE");
            env::remove_var("FUNCTION_TARGET");
            env::remove_var("REGION_NAME");
            env::remove_var("GCP_PROJECT");
        }
    }

    #[test]
    fn cloud_function_missing_name_skipped() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            env::remove_var("FUNCTION_TARGET");
            env::remove_var("FUNCTION_NAME");
            env::remove_var("K_SERVICE");
            env::set_var("FUNCTION_REGION", "us-central1");
            env::set_var("GCP_PROJECT", "my-project");
        }

        let (id, name) = build_cloud_function_identity();

        assert!(id.is_empty());
        assert!(name.is_empty());

        unsafe {
            env::remove_var("FUNCTION_REGION");
            env::remove_var("GCP_PROJECT");
        }
    }

    #[test]
    fn cloud_function_missing_project_returns_empty_id() {
        // Name present but project/region absent — resource_id empty pending metadata server.
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            env::remove_var("FUNCTION_TARGET");
            env::set_var("FUNCTION_NAME", "my-fn");
            env::remove_var("FUNCTION_REGION");
            env::remove_var("GOOGLE_CLOUD_REGION");
            env::remove_var("REGION_NAME");
            env::remove_var("GCP_PROJECT");
            env::remove_var("GCLOUD_PROJECT");
            env::remove_var("GOOGLE_CLOUD_PROJECT");
        }

        let (id, name) = build_cloud_function_identity();

        assert!(
            id.is_empty(),
            "incomplete identity must produce empty resource_id"
        );
        assert_eq!(
            name, "my-fn",
            "resource_name should still be set for metadata retry"
        );

        unsafe {
            env::remove_var("FUNCTION_NAME");
        }
    }

    #[test]
    fn cloud_function_runtime_comes_from_platform() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            env::remove_var("NODE_VERSION");
            env::remove_var("JAVA_VERSION");
            env::remove_var("GO_VERSION");
            env::set_var("PYTHON_VERSION", "3.12.7");
        }

        let mut metadata = serde_json::json!({});
        enrich_cloud_function_fields(&mut metadata);

        assert_eq!(metadata["runtime"], "python");
        assert_eq!(metadata["serverless_compat_runtime_version"], "3.12.7");

        unsafe {
            env::remove_var("PYTHON_VERSION");
        }
    }

    // ── Payload structure ────────────────────────────────────────────────────

    #[test]
    fn payload_structure_azure_function() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            env::remove_var("DD_SERVERLESS_COMPAT_VERSION");
        }

        let process_id = "test-uuid-1234";
        let resource_id =
            "/subscriptions/sub/resourcegroups/rg/providers/microsoft.web/sites/my-func";
        let body = build_payload(
            process_id,
            "azure_function",
            "startup",
            resource_id,
            "my-func",
            &EnvironmentType::AzureFunction,
            None,
        )
        .expect("build_payload must not fail");

        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let obj = payload.as_object().unwrap();

        // Top-level structure.
        assert_eq!(obj["uuid"], process_id);
        assert!(obj["timestamp"].as_i64().unwrap() > 1_000_000_000_000_000_000_i64);
        assert!(!obj.contains_key("hostname"), "hostname must be absent");

        let meta = obj["agent_metadata"].as_object().unwrap();
        assert_eq!(meta["flavor"], "serverless-compat");
        assert_eq!(meta["workload_type"], "azure_function");
        assert_eq!(meta["report_reason"], "startup");
        assert_eq!(meta["resource_id"], resource_id);
        assert_eq!(meta["resource_name"], "my-func");
        assert!(meta.contains_key("serverless_compat_version"));

        // UUID must NOT appear inside agent_metadata.
        assert!(
            !meta.contains_key("uuid"),
            "uuid must not be inside agent_metadata"
        );

        // platform_version must not appear — not in the REDAPL schema.
        assert!(!meta.contains_key("platform_version"));
    }

    #[test]
    fn payload_uses_dd_serverless_compat_version_env() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            env::set_var("DD_SERVERLESS_COMPAT_VERSION", "3.7.1");
        }

        let body = build_payload(
            "pid",
            "azure_function",
            "startup",
            "/subscriptions/s/resourcegroups/r/providers/microsoft.web/sites/f",
            "f",
            &EnvironmentType::AzureFunction,
            None,
        )
        .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            payload["agent_metadata"]["serverless_compat_version"],
            "3.7.1"
        );

        unsafe {
            env::remove_var("DD_SERVERLESS_COMPAT_VERSION");
        }
    }

    #[test]
    fn payload_falls_back_to_crate_version() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            env::remove_var("DD_SERVERLESS_COMPAT_VERSION");
        }

        let body = build_payload(
            "pid",
            "azure_function",
            "startup",
            "/subscriptions/s/resourcegroups/r/providers/microsoft.web/sites/f",
            "f",
            &EnvironmentType::AzureFunction,
            None,
        )
        .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let ver = payload["agent_metadata"]["serverless_compat_version"]
            .as_str()
            .unwrap();
        assert_eq!(ver, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn periodic_report_reason_in_payload() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            env::remove_var("DD_SERVERLESS_COMPAT_VERSION");
        }

        let body = build_payload(
            "pid",
            "cloud_function",
            "periodic",
            "//cloudfunctions.googleapis.com/projects/p/locations/r/functions/fn",
            "fn",
            &EnvironmentType::CloudFunction,
            None,
        )
        .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["agent_metadata"]["report_reason"], "periodic");
    }

    // ── Inventory gate ───────────────────────────────────────────────────────

    #[test]
    fn gate_off_by_default() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            env::remove_var("DD_SERVERLESS_COMPAT_INVENTORY_ENABLED");
        }
        assert!(
            !is_inventory_enabled(),
            "gate must be off when env var is absent"
        );
    }

    #[test]
    fn gate_on_when_set_to_true() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            env::set_var("DD_SERVERLESS_COMPAT_INVENTORY_ENABLED", "true");
        }
        assert!(
            is_inventory_enabled(),
            "gate must be on when env var is 'true'"
        );
        unsafe {
            env::remove_var("DD_SERVERLESS_COMPAT_INVENTORY_ENABLED");
        }
    }

    #[test]
    fn gate_off_when_set_to_other_value() {
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe {
            env::set_var("DD_SERVERLESS_COMPAT_INVENTORY_ENABLED", "false");
        }
        assert!(
            !is_inventory_enabled(),
            "gate must be off when env var is not 'true'"
        );
        unsafe {
            env::remove_var("DD_SERVERLESS_COMPAT_INVENTORY_ENABLED");
        }
    }
}
