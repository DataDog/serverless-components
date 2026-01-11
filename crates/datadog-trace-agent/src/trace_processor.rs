// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use async_trait::async_trait;
use hyper::{http, StatusCode};
use libdd_common::hyper_migration;
use tokio::sync::mpsc::Sender;
use tracing::debug;

use libdd_trace_obfuscation::obfuscate::obfuscate_span;
use libdd_trace_protobuf::pb;
use libdd_trace_utils::trace_utils::{self};
use libdd_trace_utils::trace_utils::{EnvironmentType, SendData};
use libdd_trace_utils::tracer_payload::{TraceChunkProcessor, TracerPayloadCollection};

use crate::http_utils::{
    self, log_and_create_http_response, log_and_create_traces_success_http_response,
};
use datadog_serverless_config::Config;

const TRACER_PAYLOAD_FUNCTION_TAGS_TAG_KEY: &str = "_dd.tags.function";

#[async_trait]
pub trait TraceProcessor {
    /// Deserializes traces from a hyper request body and sends them through the provided tokio mpsc
    /// Sender.
    async fn process_traces(
        &self,
        config: Arc<Config>,
        req: hyper_migration::HttpRequest,
        tx: Sender<trace_utils::SendData>,
        mini_agent_metadata: Arc<trace_utils::MiniAgentMetadata>,
    ) -> http::Result<hyper_migration::HttpResponse>;
}

struct ChunkProcessor {
    config: Arc<Config>,
    mini_agent_metadata: Arc<trace_utils::MiniAgentMetadata>,
}

impl TraceChunkProcessor for ChunkProcessor {
    fn process(&mut self, chunk: &mut pb::TraceChunk, root_span_index: usize) {
        // Clone app_name once instead of once per span
        let app_name = self.config.app_name.clone();

        trace_utils::set_serverless_root_span_tags(
            &mut chunk.spans[root_span_index],
            app_name.clone(),
            &self.config.env_type,
        );
        for span in chunk.spans.iter_mut() {
            trace_utils::enrich_span_with_mini_agent_metadata(span, &self.mini_agent_metadata);
            trace_utils::enrich_span_with_azure_function_metadata(span);
            if let EnvironmentType::CloudFunction = &self.config.env_type {
                trace_utils::enrich_span_with_google_cloud_function_metadata(
                    span,
                    &self.mini_agent_metadata,
                    app_name.clone(),
                );
            }
            obfuscate_span(span, &self.config.obfuscation_config);
        }
    }
}
#[derive(Clone)]
pub struct ServerlessTraceProcessor {}

#[async_trait]
impl TraceProcessor for ServerlessTraceProcessor {
    async fn process_traces(
        &self,
        config: Arc<Config>,
        req: hyper_migration::HttpRequest,
        tx: Sender<trace_utils::SendData>,
        mini_agent_metadata: Arc<trace_utils::MiniAgentMetadata>,
    ) -> http::Result<hyper_migration::HttpResponse> {
        debug!("Received traces to process");
        let (parts, body) = req.into_parts();

        if let Some(response) = http_utils::verify_request_content_length(
            &parts.headers,
            config.max_request_content_length,
            "Error processing traces",
        ) {
            return response;
        }

        let tracer_header_tags = (&parts.headers).into();

        // deserialize traces from the request body, convert to protobuf structs (see trace-protobuf
        // crate)
        let (body_size, traces) = match trace_utils::get_traces_from_request_body(body).await {
            Ok(res) => res,
            Err(err) => {
                return log_and_create_http_response(
                    &format!("Error deserializing trace from request body: {err}"),
                    StatusCode::INTERNAL_SERVER_ERROR,
                );
            }
        };

        // double check content length is < max request content length in case transfer encoding is used
        if body_size > config.max_request_content_length {
            return log_and_create_http_response(
                "Error processing traces: Payload too large",
                StatusCode::PAYLOAD_TOO_LARGE,
            );
        }

        let mut payload = match trace_utils::collect_pb_trace_chunks(
            traces,
            &tracer_header_tags,
            &mut ChunkProcessor {
                config: config.clone(),
                mini_agent_metadata: mini_agent_metadata.clone(),
            },
            true, // In mini agent, we always send agentless
        ) {
            Ok(res) => res,
            Err(err) => {
                return log_and_create_traces_success_http_response(
                    &format!("Error processing trace chunks: {err}"),
                    StatusCode::INTERNAL_SERVER_ERROR,
                )
            }
        };

        // Add function_tags to payload if we can
        if let Some(function_tags) = config.tags.function_tags() {
            if let TracerPayloadCollection::V07(ref mut tracer_payloads) = payload {
                for tracer_payload in tracer_payloads {
                    tracer_payload.tags.insert(
                        TRACER_PAYLOAD_FUNCTION_TAGS_TAG_KEY.to_string(),
                        function_tags.to_string(),
                    );
                }
            }
        }

        let send_data = SendData::new(body_size, payload, tracer_header_tags, &config.trace_intake);

        // send trace payload to our trace flusher
        match tx.send(send_data).await {
            Ok(_) => {
                return log_and_create_traces_success_http_response(
                    "Successfully buffered traces to be flushed.",
                    StatusCode::OK,
                );
            }
            Err(err) => {
                return log_and_create_http_response(
                    &format!("Error sending traces to the trace flusher: {err}"),
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
    use std::{collections::HashMap, sync::Arc, time::UNIX_EPOCH};
    use tokio::sync::mpsc::{self, Receiver, Sender};

    use crate::trace_processor::{self, TraceProcessor, TRACER_PAYLOAD_FUNCTION_TAGS_TAG_KEY};
    use datadog_serverless_config::{Config, Tags};
    use libdd_common::{hyper_migration, Endpoint};
    use libdd_trace_protobuf::pb;
    use libdd_trace_utils::test_utils::{create_test_gcp_json_span, create_test_gcp_span};
    use libdd_trace_utils::trace_utils::MiniAgentMetadata;
    use libdd_trace_utils::{
        test_utils::create_test_json_span, trace_utils, tracer_payload::TracerPayloadCollection,
    };

    fn get_current_timestamp_nanos() -> i64 {
        UNIX_EPOCH.elapsed().unwrap().as_nanos() as i64
    }

    fn create_test_config() -> Config {
        Config {
            app_name: Some("dummy_function_name".to_string()),
            max_request_content_length: 10 * 1024 * 1024,
            trace_flush_interval: 3,
            stats_flush_interval: 3,
            verify_env_timeout: 100,
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
            dd_site: "datadoghq.com".to_string(),
            dd_dogstatsd_port: 8125,
            env_type: trace_utils::EnvironmentType::CloudFunction,
            os: "linux".to_string(),
            obfuscation_config: ObfuscationConfig::new().unwrap(),
            proxy_url: None,
            tags: Tags::from_env_string("env:test,service:my-service"),
        }
    }

    fn create_test_metadata() -> MiniAgentMetadata {
        MiniAgentMetadata {
            azure_spring_app_hostname: Default::default(),
            azure_spring_app_name: Default::default(),
            gcp_project_id: Some("dummy_project_id".to_string()),
            gcp_region: Some("dummy_region_west".to_string()),
            version: Some("dummy_version".to_string()),
        }
    }

    #[tokio::test]
    async fn test_process_trace() {
        let (tx, mut rx): (
            Sender<trace_utils::SendData>,
            Receiver<trace_utils::SendData>,
        ) = mpsc::channel(1);

        let start = get_current_timestamp_nanos();

        let json_span = create_test_json_span(11, 222, 333, start, false);

        let bytes = rmp_serde::to_vec(&vec![vec![json_span]]).unwrap();
        let request = Request::builder()
            .header("datadog-meta-tracer-version", "4.0.0")
            .header("datadog-meta-lang", "nodejs")
            .header("datadog-meta-lang-version", "v19.7.0")
            .header("datadog-meta-lang-interpreter", "v8")
            .header("datadog-container-id", "33")
            .header("content-length", "100")
            .body(hyper_migration::Body::from(bytes))
            .unwrap();

        let trace_processor = trace_processor::ServerlessTraceProcessor {};
        let res = trace_processor
            .process_traces(
                Arc::new(create_test_config()),
                request,
                tx,
                Arc::new(create_test_metadata()),
            )
            .await;
        assert!(res.is_ok());

        let tracer_payload = rx.recv().await;

        assert!(tracer_payload.is_some());

        let expected_tracer_payload = pb::TracerPayload {
            container_id: "33".to_string(),
            language_name: "nodejs".to_string(),
            language_version: "v19.7.0".to_string(),
            tracer_version: "4.0.0".to_string(),
            runtime_id: "test-runtime-id-value".to_string(),
            chunks: vec![pb::TraceChunk {
                priority: i8::MIN as i32,
                origin: "".to_string(),
                spans: vec![create_test_gcp_span(11, 222, 333, start, true)],
                tags: HashMap::new(),
                dropped_trace: false,
            }],
            tags: HashMap::from([(
                TRACER_PAYLOAD_FUNCTION_TAGS_TAG_KEY.to_string(),
                "env:test,service:my-service".to_string(),
            )]),
            env: "test-env".to_string(),
            hostname: "".to_string(),
            app_version: "".to_string(),
        };

        let received_payload =
            if let TracerPayloadCollection::V07(payload) = tracer_payload.unwrap().get_payloads() {
                Some(payload[0].clone())
            } else {
                None
            };
        assert_eq!(expected_tracer_payload, received_payload.unwrap());
    }

    #[tokio::test]
    async fn test_process_trace_top_level_span_set() {
        let (tx, mut rx): (
            Sender<trace_utils::SendData>,
            Receiver<trace_utils::SendData>,
        ) = mpsc::channel(1);

        let start = get_current_timestamp_nanos();

        let json_trace = vec![
            create_test_gcp_json_span(11, 333, 222, start),
            create_test_gcp_json_span(11, 222, 0, start),
            create_test_gcp_json_span(11, 444, 333, start),
        ];

        let bytes = rmp_serde::to_vec(&vec![json_trace]).unwrap();
        let request = Request::builder()
            .header("datadog-meta-tracer-version", "4.0.0")
            .header("datadog-meta-lang", "nodejs")
            .header("datadog-meta-lang-version", "v19.7.0")
            .header("datadog-meta-lang-interpreter", "v8")
            .header("datadog-container-id", "33")
            .header("content-length", "100")
            .body(hyper_migration::Body::from(bytes))
            .unwrap();

        let trace_processor = trace_processor::ServerlessTraceProcessor {};
        let res = trace_processor
            .process_traces(
                Arc::new(create_test_config()),
                request,
                tx,
                Arc::new(create_test_metadata()),
            )
            .await;
        assert!(res.is_ok());

        let tracer_payload = rx.recv().await;

        assert!(tracer_payload.is_some());

        let expected_tracer_payload = pb::TracerPayload {
            container_id: "33".to_string(),
            language_name: "nodejs".to_string(),
            language_version: "v19.7.0".to_string(),
            tracer_version: "4.0.0".to_string(),
            runtime_id: "test-runtime-id-value".to_string(),
            chunks: vec![pb::TraceChunk {
                priority: i8::MIN as i32,
                origin: "".to_string(),
                spans: vec![
                    create_test_gcp_span(11, 333, 222, start, false),
                    create_test_gcp_span(11, 222, 0, start, true),
                    create_test_gcp_span(11, 444, 333, start, false),
                ],
                tags: HashMap::new(),
                dropped_trace: false,
            }],
            tags: HashMap::from([(
                TRACER_PAYLOAD_FUNCTION_TAGS_TAG_KEY.to_string(),
                "env:test,service:my-service".to_string(),
            )]),
            env: "test-env".to_string(),
            hostname: "".to_string(),
            app_version: "".to_string(),
        };

        let received_payload =
            if let TracerPayloadCollection::V07(payload) = tracer_payload.unwrap().get_payloads() {
                Some(payload[0].clone())
            } else {
                None
            };

        assert_eq!(expected_tracer_payload, received_payload.unwrap());
    }
}
