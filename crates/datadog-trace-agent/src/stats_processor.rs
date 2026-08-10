// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;
use std::time::UNIX_EPOCH;

use async_trait::async_trait;
use http_body_util::BodyExt;
use hyper::{StatusCode, http};
use libdd_common::http_common;
use tokio::sync::mpsc::Sender;
use tracing::debug;

use libdd_trace_protobuf::pb;
use libdd_trace_utils::stats_utils;

use crate::config::Config;
use crate::http_utils::{self, log_and_create_http_response};

#[async_trait]
pub trait StatsProcessor {
    /// Deserializes trace stats from a hyper request body and sends them through
    /// the provided tokio mpsc Sender.
    async fn process_stats(
        &self,
        config: Arc<Config>,
        req: http_common::HttpRequest,
        tx: Sender<pb::ClientStatsPayload>,
    ) -> http::Result<http_common::HttpResponse>;
}

#[derive(Clone)]
pub struct ServerlessStatsProcessor {}

#[async_trait]
impl StatsProcessor for ServerlessStatsProcessor {
    async fn process_stats(
        &self,
        config: Arc<Config>,
        req: http_common::HttpRequest,
        tx: Sender<pb::ClientStatsPayload>,
    ) -> http::Result<http_common::HttpResponse> {
        debug!("Received trace stats to process");

        let (parts, body) = req.into_parts();

        // When the agent computes trace stats itself, tracer computed stats sent to this
        // endpoint are redundant and are dropped. The body is still drained so the
        // connection can be kept alive for the next request instead of being closed.
        if config.agent_stats_computation_enabled {
            let _ = body.collect().await;
            return log_and_create_http_response(
                "Dropping trace stats: agent stats computation is enabled",
                StatusCode::ACCEPTED,
            );
        }

        if let Some(response) = http_utils::verify_request_content_length(
            &parts.headers,
            config.max_request_content_length,
            "Error processing trace stats",
        ) {
            return response;
        }

        // deserialize trace stats from the request body, convert to protobuf structs (see
        // trace-protobuf crate)
        let mut stats: pb::ClientStatsPayload =
            match stats_utils::get_stats_from_request_body(body).await {
                Ok(res) => res,
                Err(err) => {
                    return log_and_create_http_response(
                        &format!("Error deserializing trace stats from request body: {err}"),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    );
                }
            };

        if !stats.stats.is_empty() {
            let timestamp = UNIX_EPOCH.elapsed().unwrap_or_default().as_nanos();
            stats.stats[0].start = timestamp as u64;
        }

        // send trace payload to our trace flusher
        match tx.send(stats).await {
            Ok(_) => {
                return log_and_create_http_response(
                    "Successfully buffered stats to be flushed.",
                    StatusCode::ACCEPTED,
                );
            }
            Err(err) => {
                return log_and_create_http_response(
                    &format!("Error sending stats to the stats flusher: {err}"),
                    StatusCode::INTERNAL_SERVER_ERROR,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use hyper::Request;
    use libdd_trace_obfuscation::obfuscation_config::ObfuscationConfig;
    use tokio::sync::mpsc;

    use crate::config::{Config, Tags};
    use crate::peer_tags::peer_tag_keys;
    use crate::stats_processor::{ServerlessStatsProcessor, StatsProcessor};
    use libdd_common::{Endpoint, http_common};
    use libdd_trace_utils::trace_utils;

    fn create_test_config(agent_stats_computation_enabled: bool) -> Config {
        Config {
            app_name: Some("dummy_function_name".to_string()),
            max_request_content_length: 10 * 1024 * 1024,
            trace_flush_interval_secs: 3,
            stats_flush_interval_secs: 3,
            proxy_request_timeout_secs: 30,
            proxy_request_max_retries: 3,
            proxy_request_retry_backoff_base_ms: 100,
            verify_env_timeout_ms: 100,
            trace_intake: Endpoint {
                url: hyper::Uri::from_static("https://trace.agent.notdog.com/traces"),
                api_key: Some("dummy_api_key".into()),
                ..Default::default()
            },
            trace_stats_intake: Endpoint {
                url: hyper::Uri::from_static("https://trace.agent.notdog.com/stats"),
                api_key: Some("dummy_api_key".into()),
                ..Default::default()
            },
            dsm_intake: Endpoint {
                url: hyper::Uri::from_static(
                    "https://trace.agent.notdog.com/api/v0.1/pipeline_stats",
                ),
                api_key: Some("dummy_api_key".into()),
                ..Default::default()
            },
            dd_site: "datadoghq.com".to_string(),
            dd_apm_receiver_port: 8126,
            #[cfg(any(all(windows, feature = "windows-pipes"), test))]
            dd_apm_windows_pipe_name: None,
            dd_dogstatsd_port: 8125,
            #[cfg(any(all(windows, feature = "windows-pipes"), test))]
            dd_dogstatsd_windows_pipe_name: None,
            env_type: trace_utils::EnvironmentType::CloudFunction,
            os: "linux".to_string(),
            obfuscation_config: ObfuscationConfig::new().unwrap(),
            proxy_url: None,
            profiling_intake: Endpoint {
                url: hyper::Uri::from_static("https://proxy.agent.notdog.com/proxy"),
                api_key: Some("dummy_api_key".into()),
                ..Default::default()
            },
            tags: Tags::from_env_string("env:test,service:my-service"),
            env: "test-env".to_string(),
            peer_tags: peer_tag_keys().unwrap(),
            experimental_features_enabled: false,
            additional_metric_tags: vec![],
            additional_metric_tags_cardinality_limit: None,
            agent_stats_computation_enabled,
        }
    }

    // When agent stats computation is enabled, tracer computed stats are dropped rather than
    // parsed. This body is not valid msgpack, so the test also verifies the request body is never
    // fed into the stats deserializer on this path.
    #[tokio::test]
    async fn test_process_stats_drops_and_drains_when_agent_computes_stats() {
        let config = std::sync::Arc::new(create_test_config(true));
        let (tx, mut rx) = mpsc::channel(1);

        let request = Request::builder()
            .body(http_common::Body::from_bytes(
                hyper::body::Bytes::from_static(b"not valid msgpack"),
            ))
            .unwrap();

        let response = ServerlessStatsProcessor {}
            .process_stats(config, request, tx)
            .await
            .unwrap();

        assert_eq!(response.status(), hyper::StatusCode::ACCEPTED);
        // nothing should have been sent to the stats flusher on this path
        assert!(rx.try_recv().is_err());
        drop(rx);
    }
}
