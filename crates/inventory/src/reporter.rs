// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use crate::{ProcessEnv, build_inventory_report, payload, platform};
use datadog_fips::reqwest_adapter::create_reqwest_client_builder;
use libdd_common::azure_app_services::QueryEnv;
use libdd_trace_utils::trace_utils::EnvironmentType;
use std::time::Duration;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{debug, warn};

const INVENTORY_INTERVAL: Duration = Duration::from_secs(30 * 60);
const MAX_ATTEMPTS: u32 = 3;

struct Reporter {
    client: reqwest::Client,
    api_key: String,
    dd_site: String,
    intake_url: reqwest::Url,
    user_agent: String,
    platform: platform::PlatformData,
    process_id: String,
}

/// Runs the inventory reporter for the lifetime of the mini-agent.
///
/// A report is sent at startup and every 30 minutes afterward. Failures are
/// logged and contained so inventory never blocks agent startup or processing.
pub async fn run_inventory_reporter(
    api_key: Option<&str>,
    dd_site: &str,
    https_proxy: Option<&str>,
    env_type: EnvironmentType,
) {
    if !is_inventory_enabled(&ProcessEnv) || !is_supported(&env_type) {
        return;
    }

    let Some(api_key) = api_key else {
        warn!("DD_API_KEY not set, skipping inventory reporter");
        return;
    };

    let intake_url = match build_intake_url(dd_site) {
        Ok(url) => url,
        Err(error) => {
            warn!("inventory: invalid DD_SITE, skipping reporter: {error}");
            return;
        }
    };

    let Some(platform) = platform::collect(&env_type).await else {
        warn!("inventory: required cloud identity unavailable, skipping reporter");
        return;
    };

    let client = match build_client(https_proxy) {
        Ok(client) => client,
        Err(error) => {
            warn!("inventory: failed to create HTTP client: {error}");
            return;
        }
    };

    let reporter = Reporter {
        client,
        api_key: api_key.to_string(),
        dd_site: dd_site.to_string(),
        intake_url,
        user_agent: format!(
            "datadog-serverless-compat/{}",
            payload::serverless_compat_binary_version().unwrap_or("unknown")
        ),
        platform,
        // Reuse a stable UUID for every report emitted by this process.
        process_id: uuid::Uuid::new_v4().to_string(),
    };
    reporter.send_report("startup").await;

    let mut ticker = interval(INVENTORY_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ticker.tick().await; // Consume the immediate first tick.
    loop {
        ticker.tick().await;
        reporter.send_report("periodic").await;
    }
}

fn is_inventory_enabled(env: &impl QueryEnv) -> bool {
    env.get_var("DD_SERVERLESS_COMPAT_INVENTORY_ENABLED")
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
}

fn is_supported(env_type: &EnvironmentType) -> bool {
    matches!(
        env_type,
        EnvironmentType::AzureFunction | EnvironmentType::CloudFunction
    )
}

impl Reporter {
    async fn send_report(&self, report_reason: &str) {
        let report = match build_inventory_report(
            &self.platform,
            &self.process_id,
            report_reason,
            &self.dd_site,
        ) {
            Ok(report) => report,
            Err(error) => {
                warn!("inventory: failed to serialize payload: {error}");
                return;
            }
        };

        for attempt in 0..MAX_ATTEMPTS {
            match do_send(
                &self.client,
                &self.intake_url,
                &self.api_key,
                &self.user_agent,
                report.body.clone(),
            )
            .await
            {
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
}

fn should_retry_status(status: u16) -> bool {
    status == 429 || (500..=599).contains(&status)
}

fn retry_backoff(attempt: u32) -> Duration {
    Duration::from_secs(1 << attempt)
}

async fn do_send(
    client: &reqwest::Client,
    url: &reqwest::Url,
    api_key: &str,
    user_agent: &str,
    body: Vec<u8>,
) -> Result<u16, reqwest::Error> {
    let response = client
        .post(url.clone())
        .header("DD-API-KEY", api_key)
        .header("Content-Type", "application/json")
        .header("User-Agent", user_agent)
        .body(body)
        .send()
        .await?;
    Ok(response.status().as_u16())
}

fn build_client(https_proxy: Option<&str>) -> Result<reqwest::Client, Box<dyn std::error::Error>> {
    let mut builder = create_reqwest_client_builder()?
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10));
    if let Some(proxy) = https_proxy {
        builder = builder.proxy(reqwest::Proxy::https(proxy)?);
    }
    Ok(builder.build()?)
}

fn build_intake_url(dd_site: &str) -> Result<reqwest::Url, String> {
    if dd_site.is_empty()
        || !dd_site
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
    {
        return Err(format!("invalid site: {dd_site}"));
    }

    reqwest::Url::parse(&format!("https://api.{dd_site}/api/v1/metadata"))
        .map_err(|error| error.to_string())
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
    fn inventory_gate_accepts_case_insensitive_true() {
        assert!(is_inventory_enabled(&FakeEnv::new(&[(
            "DD_SERVERLESS_COMPAT_INVENTORY_ENABLED",
            "true"
        ),])));
        assert!(is_inventory_enabled(&FakeEnv::new(&[(
            "DD_SERVERLESS_COMPAT_INVENTORY_ENABLED",
            "TRUE"
        ),])));
        assert!(!is_inventory_enabled(&FakeEnv::new(&[(
            "DD_SERVERLESS_COMPAT_INVENTORY_ENABLED",
            "1"
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

    #[test]
    fn validates_inventory_intake_site() {
        assert_eq!(
            build_intake_url("us5.datadoghq.com").unwrap().as_str(),
            "https://api.us5.datadoghq.com/api/v1/metadata"
        );
        assert!(build_intake_url("datadoghq.com@attacker.example").is_err());
        assert!(build_intake_url("datadoghq.com/path").is_err());
    }

    #[tokio::test]
    async fn sends_required_headers_without_following_redirects() {
        use httpmock::prelude::*;

        let server = MockServer::start_async().await;
        let target = server
            .mock_async(|when, then| {
                when.method(POST).path("/target");
                then.status(200);
            })
            .await;
        let redirect = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/api/v1/metadata")
                    .header("DD-API-KEY", "test-key")
                    .header("Content-Type", "application/json")
                    .header("User-Agent", "datadog-serverless-compat/1.2.3")
                    .body("{}");
                then.status(307).header("Location", server.url("/target"));
            })
            .await;

        let status = do_send(
            &build_client(None).unwrap(),
            &reqwest::Url::parse(&server.url("/api/v1/metadata")).unwrap(),
            "test-key",
            "datadog-serverless-compat/1.2.3",
            b"{}".to_vec(),
        )
        .await
        .unwrap();

        assert_eq!(status, 307);
        redirect.assert_async().await;
        target.assert_calls_async(0).await;
    }
}
