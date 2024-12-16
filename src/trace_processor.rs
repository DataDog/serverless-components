// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use async_trait::async_trait;
use hyper::{http, Body, Request, Response, StatusCode};
use tokio::sync::mpsc::Sender;
use tracing::debug;

use datadog_trace_obfuscation::obfuscate::obfuscate_span;
use datadog_trace_protobuf::pb;
use datadog_trace_utils::trace_utils::SendData;
use datadog_trace_utils::trace_utils::{self};
use datadog_trace_utils::tracer_payload::{TraceChunkProcessor, TraceCollection};

use crate::{
    config::Config,
    http_utils::{self, log_and_create_http_response, log_and_create_traces_success_http_response},
};

#[async_trait]
pub trait TraceProcessor {
    /// Deserializes traces from a hyper request body and sends them through the provided tokio mpsc
    /// Sender.
    async fn process_traces(
        &self,
        config: Arc<Config>,
        req: Request<Body>,
        tx: Sender<trace_utils::SendData>,
        mini_agent_metadata: Arc<trace_utils::MiniAgentMetadata>,
    ) -> http::Result<Response<Body>>;
}

struct ChunkProcessor {
    config: Arc<Config>,
    mini_agent_metadata: Arc<trace_utils::MiniAgentMetadata>,
}

impl TraceChunkProcessor for ChunkProcessor {
    fn process(&mut self, chunk: &mut pb::TraceChunk, root_span_index: usize) {
        trace_utils::set_serverless_root_span_tags(
            &mut chunk.spans[root_span_index],
            self.config.app_name.clone(),
            &self.config.env_type,
        );
        for span in chunk.spans.iter_mut() {
            trace_utils::enrich_span_with_mini_agent_metadata(span, &self.mini_agent_metadata);
            trace_utils::enrich_span_with_azure_function_metadata(span);
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
        req: Request<Body>,
        tx: Sender<trace_utils::SendData>,
        mini_agent_metadata: Arc<trace_utils::MiniAgentMetadata>,
    ) -> http::Result<Response<Body>> {
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

        let payload = trace_utils::collect_trace_chunks(
            TraceCollection::V07(traces),
            &tracer_header_tags,
            &mut ChunkProcessor {
                config: config.clone(),
                mini_agent_metadata: mini_agent_metadata.clone(),
            },
            true, // In mini agent, we always send agentless
        );

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
    use datadog_trace_obfuscation::obfuscation_config::ObfuscationConfig;
    use hyper::Request;
    use std::{collections::HashMap, sync::Arc, time::UNIX_EPOCH};
    use tokio::sync::mpsc::{self, Receiver, Sender};

    use crate::{
        config::Config,
        trace_processor::{self, TraceProcessor},
    };
    use datadog_trace_protobuf::pb;
    use datadog_trace_utils::{
        test_utils::{create_test_json_span, create_test_span},
        trace_utils,
        tracer_payload::TracerPayloadCollection,
    };
    use ddcommon::Endpoint;

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
        }
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_process_trace() {
        let (tx, mut rx): (
            Sender<trace_utils::SendData>,
            Receiver<trace_utils::SendData>,
        ) = mpsc::channel(1);

        let start = get_current_timestamp_nanos();

        let json_span = create_test_json_span(11, 222, 333, start);

        let bytes = rmp_serde::to_vec(&vec![vec![json_span]]).unwrap();
        let request = Request::builder()
            .header("datadog-meta-tracer-version", "4.0.0")
            .header("datadog-meta-lang", "nodejs")
            .header("datadog-meta-lang-version", "v19.7.0")
            .header("datadog-meta-lang-interpreter", "v8")
            .header("datadog-container-id", "33")
            .header("content-length", "100")
            .body(hyper::body::Body::from(bytes))
            .unwrap();

        let trace_processor = trace_processor::ServerlessTraceProcessor {};
        let res = trace_processor
            .process_traces(
                Arc::new(create_test_config()),
                request,
                tx,
                Arc::new(trace_utils::MiniAgentMetadata::default()),
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
                spans: vec![create_test_span(11, 222, 333, start, true)],
                tags: HashMap::new(),
                dropped_trace: false,
            }],
            tags: HashMap::new(),
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
    #[cfg_attr(miri, ignore)]
    async fn test_process_trace_top_level_span_set() {
        let (tx, mut rx): (
            Sender<trace_utils::SendData>,
            Receiver<trace_utils::SendData>,
        ) = mpsc::channel(1);

        let start = get_current_timestamp_nanos();

        let json_trace = vec![
            create_test_json_span(11, 333, 222, start),
            create_test_json_span(11, 222, 0, start),
            create_test_json_span(11, 444, 333, start),
        ];

        let bytes = rmp_serde::to_vec(&vec![json_trace]).unwrap();
        let request = Request::builder()
            .header("datadog-meta-tracer-version", "4.0.0")
            .header("datadog-meta-lang", "nodejs")
            .header("datadog-meta-lang-version", "v19.7.0")
            .header("datadog-meta-lang-interpreter", "v8")
            .header("datadog-container-id", "33")
            .header("content-length", "100")
            .body(hyper::body::Body::from(bytes))
            .unwrap();

        let trace_processor = trace_processor::ServerlessTraceProcessor {};
        let res = trace_processor
            .process_traces(
                Arc::new(create_test_config()),
                request,
                tx,
                Arc::new(trace_utils::MiniAgentMetadata::default()),
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
                    create_test_span(11, 333, 222, start, false),
                    create_test_span(11, 222, 0, start, true),
                    create_test_span(11, 444, 333, start, false),
                ],
                tags: HashMap::new(),
                dropped_trace: false,
            }],
            tags: HashMap::new(),
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
