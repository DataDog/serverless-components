// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use crate::{ProcessEnv, build_inventory_report};
use datadog_fips::reqwest_adapter::create_reqwest_client_builder;
use libdd_common::azure_app_services::QueryEnv;
use libdd_trace_utils::trace_utils::EnvironmentType;
use std::time::Duration;
use tokio::time::interval;
use tracing::{debug, warn};

const INVENTORY_INTERVAL: Duration = Duration::from_secs(30 * 60);
const MAX_ATTEMPTS: u32 = 3;

/// Runs the inventory reporter for the lifetime of the mini-agent.
///
/// A report is sent at startup and every 30 minutes afterward. Failures are
/// logged and contained so inventory never blocks agent startup or processing.
pub async fn run_inventory_reporter(
    api_key: &str,
    dd_site: &str,
    https_proxy: Option<&str>,
    env_type: EnvironmentType,
) {
    if !is_inventory_enabled(&ProcessEnv) || !is_supported(&env_type) {
        return;
    }

    let client = match build_client(https_proxy) {
        Ok(client) => client,
        Err(error) => {
            warn!("inventory: failed to create HTTP client: {error}");
            return;
        }
    };

    // Reuse a stable UUID for every report emitted by this process.
    let process_id = uuid::Uuid::new_v4().to_string();
    send_report(&client, api_key, dd_site, &env_type, &process_id, "startup").await;

    let mut ticker = interval(INVENTORY_INTERVAL);
    ticker.tick().await; // Consume the immediate first tick.
    loop {
        ticker.tick().await;
        send_report(
            &client,
            api_key,
            dd_site,
            &env_type,
            &process_id,
            "periodic",
        )
        .await;
    }
}

fn is_inventory_enabled(env: &impl QueryEnv) -> bool {
    env.get_var("DD_SERVERLESS_COMPAT_INVENTORY_ENABLED")
        .as_deref()
        == Some("true")
}

fn is_supported(env_type: &EnvironmentType) -> bool {
    matches!(
        env_type,
        EnvironmentType::AzureFunction | EnvironmentType::CloudFunction
    )
}

async fn send_report(
    client: &reqwest::Client,
    api_key: &str,
    dd_site: &str,
    env_type: &EnvironmentType,
    process_id: &str,
    report_reason: &str,
) {
    let report = match build_inventory_report(env_type, process_id, report_reason).await {
        Ok(Some(report)) => report,
        Ok(None) => {
            warn!(
                "inventory: required cloud identity unavailable, skipping {report_reason} report"
            );
            return;
        }
        Err(error) => {
            warn!("inventory: failed to serialize payload: {error}");
            return;
        }
    };

    let url = format!("https://api.{dd_site}/api/v1/metadata");
    for attempt in 0..MAX_ATTEMPTS {
        match do_send(client, &url, api_key, report.body.clone()).await {
            Ok(status) if status < 300 => {
                debug!(
                    "inventory: report sent (report_reason={report_reason}, workload_type={}, resource_id={}, status={status})",
                    report.workload_type, report.resource_id
                );
                return;
            }
            Ok(status) if should_retry_status(status) && attempt + 1 < MAX_ATTEMPTS => {
                let backoff = retry_backoff(attempt);
                warn!(
                    "inventory: transient failure, retrying in {backoff:?} (report_reason={report_reason}, attempt={}, status={status})",
                    attempt + 1
                );
                tokio::time::sleep(backoff).await;
            }
            Ok(status) => {
                warn!(
                    "inventory: intake rejected report (report_reason={report_reason}, workload_type={}, status={status})",
                    report.workload_type
                );
                return;
            }
            Err(error) if attempt + 1 < MAX_ATTEMPTS => {
                let backoff = retry_backoff(attempt);
                warn!(
                    "inventory: transport error, retrying in {backoff:?} (report_reason={report_reason}, attempt={}, error={error})",
                    attempt + 1
                );
                tokio::time::sleep(backoff).await;
            }
            Err(error) => {
                warn!(
                    "inventory: transport error after {MAX_ATTEMPTS} attempts (report_reason={report_reason}, error={error})"
                );
                return;
            }
        }
    }
}

fn should_retry_status(status: u16) -> bool {
    status == 429 || (500..=599).contains(&status)
}

fn retry_backoff(attempt: u32) -> Duration {
    Duration::from_secs(1 << attempt)
}

async fn do_send(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    body: Vec<u8>,
) -> Result<u16, reqwest::Error> {
    let response = client
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
    Ok(response.status().as_u16())
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
    use crate::test_env::FakeEnv;

    #[test]
    fn inventory_gate_is_off_by_default() {
        assert!(!is_inventory_enabled(&FakeEnv::default()));
    }

    #[test]
    fn inventory_gate_requires_exact_true_value() {
        assert!(is_inventory_enabled(&FakeEnv::new(&[(
            "DD_SERVERLESS_COMPAT_INVENTORY_ENABLED",
            "true"
        ),])));
        assert!(!is_inventory_enabled(&FakeEnv::new(&[(
            "DD_SERVERLESS_COMPAT_INVENTORY_ENABLED",
            "TRUE"
        ),])));
    }

    #[test]
    fn only_compat_inventory_workloads_are_supported() {
        assert!(is_supported(&EnvironmentType::AzureFunction));
        assert!(is_supported(&EnvironmentType::CloudFunction));
        assert!(!is_supported(&EnvironmentType::LambdaFunction));
        assert!(!is_supported(&EnvironmentType::AzureSpringApp));
    }

    #[test]
    fn retries_rate_limits_and_server_errors() {
        assert!(should_retry_status(429));
        assert!(should_retry_status(500));
        assert!(should_retry_status(599));
        assert!(!should_retry_status(400));
        assert!(!should_retry_status(600));
    }

    #[test]
    fn retry_backoff_is_bounded_exponential() {
        assert_eq!(retry_backoff(0), Duration::from_secs(1));
        assert_eq!(retry_backoff(1), Duration::from_secs(2));
        assert_eq!(retry_backoff(2), Duration::from_secs(4));
    }
}
