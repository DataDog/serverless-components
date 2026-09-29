// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use super::PlatformData;
use datadog_fips::reqwest_adapter::create_reqwest_client_builder;
use serde_json::{Map, Value};
use std::{env, time::Duration};
use tracing::{debug, warn};

const METADATA_BASE_URL: &str = "http://metadata.google.internal/computeMetadata/v1";

struct Identity {
    name: String,
    region: Option<String>,
    project: Option<String>,
}

pub(super) async fn collect() -> Option<PlatformData> {
    let mut identity = identity_from_env()?;

    match (&identity.region, &identity.project) {
        (None, None) => {
            let (region, project) = tokio::join!(fetch_region(), fetch_project());
            identity.region = region;
            identity.project = project;
        }
        (None, Some(_)) => identity.region = fetch_region().await,
        (Some(_), None) => identity.project = fetch_project().await,
        (Some(_), Some(_)) => {}
    }

    build_platform_data(identity)
}

fn build_platform_data(identity: Identity) -> Option<PlatformData> {
    let region = identity.region?;
    let project = identity.project?;
    let resource_id = format!(
        "//cloudfunctions.googleapis.com/projects/{project}/locations/{region}/functions/{}",
        identity.name
    );

    let mut metadata = Map::new();
    metadata.insert("region".into(), Value::String(region));
    metadata.insert("gcp_project_id".into(), Value::String(project));

    if let Some((runtime, version)) = detect_runtime() {
        metadata.insert("runtime".into(), Value::String(runtime.into()));
        metadata.insert(
            "serverless_compat_runtime_version".into(),
            Value::String(version),
        );
    }

    Some(PlatformData {
        workload_type: "cloud_function",
        resource_id,
        resource_name: identity.name,
        metadata,
    })
}

fn identity_from_env() -> Option<Identity> {
    // The compat binary is only installed in Gen1 Cloud Functions. Newer Gen1
    // runtimes can expose FUNCTION_TARGET and K_SERVICE because they run on
    // Cloud Run infrastructure, so FUNCTION_TARGET cannot distinguish Gen2.
    let name = first_env(&["FUNCTION_NAME", "K_SERVICE"])?;

    Some(Identity {
        name,
        region: first_env(&["FUNCTION_REGION", "GOOGLE_CLOUD_REGION", "REGION_NAME"]),
        project: first_env(&["GCP_PROJECT", "GCLOUD_PROJECT", "GOOGLE_CLOUD_PROJECT"]),
    })
}

fn first_env(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn detect_runtime() -> Option<(&'static str, String)> {
    for (runtime, variable) in [
        ("node", "NODE_VERSION"),
        ("python", "PYTHON_VERSION"),
        ("java", "JAVA_VERSION"),
        ("go", "GO_VERSION"),
    ] {
        if let Some(version) = first_env(&[variable]) {
            return Some((runtime, version));
        }
    }
    None
}

async fn fetch_metadata_value(
    path: &str,
    label: &str,
    parse: impl FnOnce(&str) -> Option<String>,
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

    let response = match client
        .get(format!("{METADATA_BASE_URL}/{path}"))
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

    if !response.status().is_success() {
        warn!(
            "inventory: GCP metadata server returned {} for {label}",
            response.status()
        );
        return None;
    }

    let body = match response.text().await {
        Ok(body) => body,
        Err(error) => {
            warn!("inventory: failed to read GCP metadata {label}: {error}");
            return None;
        }
    };
    let value = parse(body.trim());
    debug!("inventory: GCP metadata server {label}: {value:?}");
    value
}

async fn fetch_region() -> Option<String> {
    // Response: projects/<project-number>/regions/<region-name>
    fetch_metadata_value("instance/region", "region", |body| {
        body.split('/')
            .next_back()
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
    .await
}

async fn fetch_project() -> Option<String> {
    fetch_metadata_value("project/project-id", "project-id", |body| {
        (!body.is_empty()).then(|| body.to_string())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::ENV_LOCK;
    use std::sync::MutexGuard;

    const GCP_ENV: &[&str] = &[
        "FUNCTION_NAME",
        "K_SERVICE",
        "FUNCTION_TARGET",
        "FUNCTION_REGION",
        "GOOGLE_CLOUD_REGION",
        "REGION_NAME",
        "GCP_PROJECT",
        "GCLOUD_PROJECT",
        "GOOGLE_CLOUD_PROJECT",
        "NODE_VERSION",
        "PYTHON_VERSION",
        "JAVA_VERSION",
        "GO_VERSION",
    ];

    fn clean_env() -> MutexGuard<'static, ()> {
        let lock = ENV_LOCK.lock().unwrap();
        for variable in GCP_ENV {
            unsafe { env::remove_var(variable) };
        }
        lock
    }

    #[test]
    fn reads_complete_gen1_identity() {
        let _lock = clean_env();
        unsafe {
            env::set_var("FUNCTION_NAME", "my-fn");
            env::set_var("FUNCTION_REGION", "us-central1");
            env::set_var("GCP_PROJECT", "my-project");
        }

        let identity = identity_from_env().unwrap();
        assert_eq!(identity.name, "my-fn");
        assert_eq!(identity.region.as_deref(), Some("us-central1"));
        assert_eq!(identity.project.as_deref(), Some("my-project"));
    }

    #[test]
    fn builds_complete_inventory_data() {
        let _lock = clean_env();
        unsafe {
            env::set_var("FUNCTION_NAME", "my-fn");
            env::set_var("FUNCTION_REGION", "us-central1");
            env::set_var("GCP_PROJECT", "my-project");
            env::set_var("PYTHON_VERSION", "3.12.7");
        }

        let data = build_platform_data(identity_from_env().unwrap()).unwrap();
        assert_eq!(data.workload_type, "cloud_function");
        assert_eq!(
            data.resource_id,
            "//cloudfunctions.googleapis.com/projects/my-project/locations/us-central1/functions/my-fn"
        );
        assert_eq!(data.metadata["runtime"], "python");
        assert_eq!(data.metadata["serverless_compat_runtime_version"], "3.12.7");
    }

    #[test]
    fn uses_region_and_project_fallbacks() {
        let _lock = clean_env();
        unsafe {
            env::set_var("FUNCTION_NAME", "my-fn");
            env::set_var("GOOGLE_CLOUD_REGION", "europe-west1");
            env::set_var("GOOGLE_CLOUD_PROJECT", "my-project");
        }

        let identity = identity_from_env().unwrap();
        assert_eq!(identity.region.as_deref(), Some("europe-west1"));
        assert_eq!(identity.project.as_deref(), Some("my-project"));
    }

    #[test]
    fn newer_gen1_runtime_uses_k_service() {
        let _lock = clean_env();
        unsafe {
            env::set_var("K_SERVICE", "my-service");
            env::set_var("FUNCTION_TARGET", "my-handler");
        }

        let identity = identity_from_env().unwrap();
        assert_eq!(identity.name, "my-service");
    }

    #[test]
    fn missing_name_is_not_an_inventory_workload() {
        let _lock = clean_env();
        unsafe {
            env::set_var("FUNCTION_REGION", "us-central1");
            env::set_var("GCP_PROJECT", "my-project");
        }

        assert!(identity_from_env().is_none());
    }

    #[test]
    fn incomplete_identity_is_retained_for_metadata_lookup() {
        let _lock = clean_env();
        unsafe { env::set_var("FUNCTION_NAME", "my-fn") };

        let identity = identity_from_env().unwrap();
        assert_eq!(identity.name, "my-fn");
        assert!(identity.region.is_none());
        assert!(identity.project.is_none());
    }

    #[test]
    fn detects_runtime_from_platform_environment() {
        let _lock = clean_env();
        unsafe { env::set_var("PYTHON_VERSION", "3.12.7") };

        assert_eq!(detect_runtime(), Some(("python", "3.12.7".into())));
    }

    #[test]
    fn parses_metadata_region() {
        let body = "projects/123456/regions/us-central1";
        let region = body.split('/').next_back().map(str::to_string);
        assert_eq!(region.as_deref(), Some("us-central1"));
    }
}
