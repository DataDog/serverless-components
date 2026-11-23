// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! The agent's own telemetry, which the Go agent's aggregator sends at every metrics flush: the
//! `datadog.agent.up` check run and the `datadog.agent.running` gauge.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use dogstatsd::datadog::MetricsIntakeUrlPrefix;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::warn;

/// The version tag of `datadog.agent.running`.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Sends the agent's own telemetry to the metrics intake. Clones share one lock, so sends from a
/// periodic flush and the shutdown flush never overlap, as the Go agent runs every flush in one
/// flush loop.
#[derive(Clone)]
pub struct AgentTelemetry {
    client: reqwest::Client,
    api_key: String,
    series_url: String,
    check_run_url: String,
    send_lock: Arc<Mutex<()>>,
}

impl AgentTelemetry {
    pub fn new(
        client: reqwest::Client,
        api_key: String,
        intake_url_prefix: &MetricsIntakeUrlPrefix,
    ) -> Self {
        Self {
            client,
            api_key,
            series_url: format!("{intake_url_prefix}/api/v2/series"),
            check_run_url: format!("{intake_url_prefix}/api/v1/check_run"),
            send_lock: Arc::default(),
        }
    }

    /// Sends the check run and the gauge, both stamped with the current time.
    pub async fn send(&self) {
        let _guard = self.send_lock.lock().await;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or_default();
        tokio::join!(
            self.post(&self.series_url, running_series(VERSION, timestamp)),
            self.post(&self.check_run_url, up_check_run(timestamp)),
        );
    }

    async fn post(&self, url: &str, body: Value) {
        let result = self
            .client
            .post(url)
            .header("DD-API-KEY", &self.api_key)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await;
        match result {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => warn!(
                "Failed to send the agent telemetry to {url}: status {}",
                response.status()
            ),
            Err(error) => warn!("Failed to send the agent telemetry to {url}: {error}"),
        }
    }
}

/// The series API v2 payload of `datadog.agent.running`, as the Go agent sends it: a gauge (type
/// 3) of the flush time with no host and no interval, from the Agent origin product (10).
fn running_series(version: &str, timestamp: u64) -> Value {
    json!({
        "series": [{
            "metric": "datadog.agent.running",
            "type": 3,
            "points": [{"timestamp": timestamp, "value": 1.0}],
            "tags": [format!("version:{version}")],
            "source_type_name": "System",
            "metadata": {"origin": {"origin_product": 10}},
        }]
    })
}

/// The check run API v1 payload of `datadog.agent.up` with the status OK (0), as the Go agent
/// sends it.
fn up_check_run(timestamp: u64) -> Value {
    json!([{
        "check": "datadog.agent.up",
        "host_name": "",
        "timestamp": timestamp,
        "status": 0,
        "message": "",
        "tags": null,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads_match_the_go_agent() {
        let cases = [
            (
                running_series("1.2.3", 1_795_415_763),
                r#"{"series":[{"metadata":{"origin":{"origin_product":10}},"metric":"datadog.agent.running","points":[{"timestamp":1795415763,"value":1.0}],"source_type_name":"System","tags":["version:1.2.3"],"type":3}]}"#,
            ),
            (
                up_check_run(1_795_415_763),
                r#"[{"check":"datadog.agent.up","host_name":"","message":"","status":0,"tags":null,"timestamp":1795415763}]"#,
            ),
        ];
        for (payload, expected) in cases {
            let expected: Value = serde_json::from_str(expected).unwrap();
            assert_eq!(payload, expected);
        }
    }

    /// Two sends that overlap, such as a periodic flush and the shutdown flush, run one after the
    /// other, as the Go agent's single flush loop runs its flushes.
    #[tokio::test(flavor = "multi_thread")]
    async fn overlapping_sends_run_one_after_the_other() {
        use dogstatsd::datadog::{DdDdUrl, MetricsIntakeUrlPrefixOverride};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let in_flight = Arc::new(AtomicUsize::new(0));
        let most_in_flight = Arc::new(AtomicUsize::new(0));
        let (server_in_flight, server_most_in_flight) = (in_flight.clone(), most_in_flight.clone());
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (in_flight, most_in_flight) =
                    (server_in_flight.clone(), server_most_in_flight.clone());
                tokio::spawn(async move {
                    let mut stream = BufReader::new(stream);
                    loop {
                        let mut content_length = 0;
                        loop {
                            let mut line = String::new();
                            if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                                return;
                            }
                            if line.trim_end().is_empty() {
                                break;
                            }
                            if let Some((name, value)) = line.split_once(':')
                                && name.eq_ignore_ascii_case("content-length")
                            {
                                content_length = value.trim().parse().unwrap();
                            }
                        }
                        let mut body = vec![0; content_length];
                        stream.read_exact(&mut body).await.unwrap();
                        let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                        most_in_flight.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        in_flight.fetch_sub(1, Ordering::SeqCst);
                        let response = b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\r\n";
                        if stream.get_mut().write_all(response).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        let override_prefix =
            MetricsIntakeUrlPrefixOverride::maybe_new(None, Some(DdDdUrl::new(base_url).unwrap()));
        let prefix = MetricsIntakeUrlPrefix::new(None, override_prefix).unwrap();
        let telemetry = AgentTelemetry::new(reqwest::Client::new(), "key".to_string(), &prefix);

        let periodic = telemetry.clone();

        tokio::join!(periodic.send(), telemetry.send());

        // One send posts the series and the check run together.
        assert_eq!(most_in_flight.load(Ordering::SeqCst), 2);
    }
}
