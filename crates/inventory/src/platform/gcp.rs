// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use super::PlatformData;
use crate::ProcessEnv;
use libdd_common::azure_app_services::QueryEnv;
use serde_json::{Map, Value};
use std::time::Duration;
use tracing::{debug, warn};

const METADATA_BASE_URL: &str = "http://metadata.google.internal/computeMetadata/v1";
const MAX_METADATA_RESPONSE_BYTES: usize = 1024;

struct Identity {
    name: String,
    region: Option<String>,
    project: Option<String>,
}

pub(super) async fn collect() -> Option<PlatformData> {
    collect_from(ProcessEnv).await
}

async fn collect_from<E: QueryEnv>(env: E) -> Option<PlatformData> {
    collect_from_with_metadata_base(env, METADATA_BASE_URL).await
}

async fn collect_from_with_metadata_base<E: QueryEnv>(
    env: E,
    metadata_base_url: &str,
) -> Option<PlatformData> {
    let mut identity = identity_from_env(&env)?;

    if identity.region.is_none() || identity.project.is_none() {
        let client = build_metadata_client()?;

        match (&identity.region, &identity.project) {
            (None, None) => {
                let (region, project) = tokio::join!(
                    fetch_region(&client, metadata_base_url),
                    fetch_project(&client, metadata_base_url)
                );
                identity.region = region;
                identity.project = project;
            }
            (None, Some(_)) => identity.region = fetch_region(&client, metadata_base_url).await,
            (Some(_), None) => identity.project = fetch_project(&client, metadata_base_url).await,
            (Some(_), Some(_)) => {}
        }
    }

    build_platform_data(identity, &env)
}

fn build_platform_data(identity: Identity, env: &impl QueryEnv) -> Option<PlatformData> {
    let region = identity.region?;
    let project = identity.project?;
    let resource_id = format!(
        "//cloudfunctions.googleapis.com/projects/{project}/locations/{region}/functions/{}",
        identity.name
    );

    let mut metadata = Map::new();
    metadata.insert("region".into(), Value::String(region));
    metadata.insert("gcp_project_id".into(), Value::String(project));

    if let Some((runtime, version)) = detect_runtime(env) {
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

fn identity_from_env(env: &impl QueryEnv) -> Option<Identity> {
    // The compat binary is only installed in Gen1 Cloud Functions. Newer Gen1
    // runtimes can expose FUNCTION_TARGET and K_SERVICE because they run on
    // Cloud Run infrastructure, so FUNCTION_TARGET cannot distinguish Gen2.
    let name = first_env(env, &["FUNCTION_NAME", "K_SERVICE"])?;

    Some(Identity {
        name,
        region: first_env(
            env,
            &["FUNCTION_REGION", "GOOGLE_CLOUD_REGION", "REGION_NAME"],
        ),
        project: first_env(
            env,
            &["GCP_PROJECT", "GCLOUD_PROJECT", "GOOGLE_CLOUD_PROJECT"],
        ),
    })
}

fn first_env(env: &impl QueryEnv, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| env.get_var(name).filter(|value| !value.is_empty()))
}

fn detect_runtime(env: &impl QueryEnv) -> Option<(&'static str, String)> {
    for (runtime, variable) in [
        ("node", "NODE_VERSION"),
        ("python", "PYTHON_VERSION"),
        ("java", "JAVA_VERSION"),
        ("go", "GO_VERSION"),
    ] {
        if let Some(version) = first_env(env, &[variable]) {
            return Some((runtime, version));
        }
    }
    None
}

fn build_metadata_client() -> Option<reqwest::Client> {
    // This client is scoped to GCP's fixed, plain-HTTP metadata endpoint, so it
    // neither needs nor should configure a TLS provider.
    #[allow(clippy::disallowed_methods)]
    match reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(client) => Some(client),
        Err(error) => {
            warn!("inventory: failed to create GCP metadata client: {error}");
            None
        }
    }
}

async fn fetch_metadata_value(
    client: &reqwest::Client,
    base_url: &str,
    path: &str,
    label: &str,
    parse: impl FnOnce(&str) -> Option<String>,
) -> Option<String> {
    let mut response = match client
        .get(format!("{}/{path}", base_url.trim_end_matches('/')))
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

    let response_flavor = response
        .headers()
        .get("Metadata-Flavor")
        .and_then(|value| value.to_str().ok());
    if response_flavor != Some("Google") {
        warn!("inventory: GCP metadata response missing Metadata-Flavor header for {label}");
        return None;
    }

    if response
        .content_length()
        .is_some_and(|length| length > MAX_METADATA_RESPONSE_BYTES as u64)
    {
        warn!("inventory: GCP metadata response too large for {label}");
        return None;
    }

    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if body.len() + chunk.len() <= MAX_METADATA_RESPONSE_BYTES => {
                body.extend_from_slice(&chunk);
            }
            Ok(Some(_)) => {
                warn!("inventory: GCP metadata response too large for {label}");
                return None;
            }
            Ok(None) => break,
            Err(error) => {
                warn!("inventory: failed to read GCP metadata {label}: {error}");
                return None;
            }
        }
    }

    let body = match std::str::from_utf8(&body) {
        Ok(body) => body,
        Err(error) => {
            warn!("inventory: GCP metadata {label} was not UTF-8: {error}");
            return None;
        }
    };
    let value = parse(body.trim());
    debug!("inventory: GCP metadata server {label}: {value:?}");
    value
}

async fn fetch_region(client: &reqwest::Client, base_url: &str) -> Option<String> {
    // Response: projects/<project-number>/regions/<region-name>
    fetch_metadata_value(client, base_url, "instance/region", "region", |body| {
        body.split('/')
            .next_back()
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
    .await
}

async fn fetch_project(client: &reqwest::Client, base_url: &str) -> Option<String> {
    fetch_metadata_value(
        client,
        base_url,
        "project/project-id",
        "project-id",
        |body| (!body.is_empty()).then(|| body.to_string()),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::FakeEnv;
    use httpmock::prelude::*;

    #[test]
    fn reads_complete_gen1_identity() {
        let env = FakeEnv::new(&[
            ("FUNCTION_NAME", "my-fn"),
            ("FUNCTION_REGION", "us-central1"),
            ("GCP_PROJECT", "my-project"),
        ]);
        let identity = identity_from_env(&env).unwrap();
        assert_eq!(identity.name, "my-fn");
        assert_eq!(identity.region.as_deref(), Some("us-central1"));
        assert_eq!(identity.project.as_deref(), Some("my-project"));
    }

    #[test]
    fn builds_complete_inventory_data() {
        let env = FakeEnv::new(&[
            ("FUNCTION_NAME", "my-fn"),
            ("FUNCTION_REGION", "us-central1"),
            ("GCP_PROJECT", "my-project"),
            ("PYTHON_VERSION", "3.12.7"),
        ]);
        let data = build_platform_data(identity_from_env(&env).unwrap(), &env).unwrap();
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
        let env = FakeEnv::new(&[
            ("FUNCTION_NAME", "my-fn"),
            ("GOOGLE_CLOUD_REGION", "europe-west1"),
            ("GOOGLE_CLOUD_PROJECT", "my-project"),
        ]);
        let identity = identity_from_env(&env).unwrap();
        assert_eq!(identity.region.as_deref(), Some("europe-west1"));
        assert_eq!(identity.project.as_deref(), Some("my-project"));
    }

    #[test]
    fn newer_gen1_runtime_uses_k_service() {
        let env = FakeEnv::new(&[
            ("K_SERVICE", "my-service"),
            ("FUNCTION_TARGET", "my-handler"),
        ]);
        let identity = identity_from_env(&env).unwrap();
        assert_eq!(identity.name, "my-service");
    }

    #[test]
    fn missing_name_is_not_an_inventory_workload() {
        let env = FakeEnv::new(&[
            ("FUNCTION_REGION", "us-central1"),
            ("GCP_PROJECT", "my-project"),
        ]);
        assert!(identity_from_env(&env).is_none());
    }

    #[test]
    fn incomplete_identity_is_retained_for_metadata_lookup() {
        let env = FakeEnv::new(&[("FUNCTION_NAME", "my-fn")]);
        let identity = identity_from_env(&env).unwrap();
        assert_eq!(identity.name, "my-fn");
        assert!(identity.region.is_none());
        assert!(identity.project.is_none());
    }

    #[test]
    fn detects_runtime_from_platform_environment() {
        let env = FakeEnv::new(&[("PYTHON_VERSION", "3.12.7")]);
        assert_eq!(detect_runtime(&env), Some(("python", "3.12.7".into())));
    }

    #[tokio::test]
    async fn completes_identity_from_metadata_server() {
        let server = MockServer::start_async().await;
        let region = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/instance/region")
                    .header("Metadata-Flavor", "Google");
                then.status(200)
                    .header("Metadata-Flavor", "Google")
                    .body("projects/123/regions/us-central1\n");
            })
            .await;
        let project = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/project/project-id")
                    .header("Metadata-Flavor", "Google");
                then.status(200)
                    .header("Metadata-Flavor", "Google")
                    .body("my-project\n");
            })
            .await;

        let data = collect_from_with_metadata_base(
            FakeEnv::new(&[("FUNCTION_NAME", "my-fn")]),
            &server.base_url(),
        )
        .await
        .expect("metadata should complete the identity");

        assert_eq!(data.metadata["region"], "us-central1");
        assert_eq!(data.metadata["gcp_project_id"], "my-project");
        region.assert_async().await;
        project.assert_async().await;
    }

    #[tokio::test]
    async fn rejects_untrusted_or_oversized_metadata_responses() {
        let server = MockServer::start_async().await;
        let client = build_metadata_client().expect("metadata client should build");
        server
            .mock_async(|when, then| {
                when.path("/missing-header");
                then.status(200).body("my-project");
            })
            .await;
        server
            .mock_async(|when, then| {
                when.path("/oversized");
                then.status(200)
                    .header("Metadata-Flavor", "Google")
                    .body("x".repeat(MAX_METADATA_RESPONSE_BYTES + 1));
            })
            .await;

        assert!(
            fetch_metadata_value(
                &client,
                &server.base_url(),
                "missing-header",
                "test",
                |body| Some(body.to_string())
            )
            .await
            .is_none()
        );
        assert!(
            fetch_metadata_value(&client, &server.base_url(), "oversized", "test", |body| {
                Some(body.to_string())
            })
            .await
            .is_none()
        );
    }

    #[tokio::test]
    async fn does_not_follow_metadata_redirects() {
        let server = MockServer::start_async().await;
        let client = build_metadata_client().expect("metadata client should build");
        let target = server
            .mock_async(|when, then| {
                when.path("/target");
                then.status(200)
                    .header("Metadata-Flavor", "Google")
                    .body("my-project");
            })
            .await;
        server
            .mock_async(|when, then| {
                when.path("/redirect");
                then.status(307).header("Location", server.url("/target"));
            })
            .await;

        assert!(
            fetch_metadata_value(
                &client,
                &server.base_url(),
                "redirect",
                "test",
                |body| Some(body.to_string())
            )
            .await
            .is_none()
        );
        target.assert_calls_async(0).await;
    }
}
