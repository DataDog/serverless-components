// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Config load benchmarks. Run with `cargo bench -p datadog-agent-config`.
//!
//! The environment is process-global, so each bench sets its scenario once,
//! outside the timed loop. No tracing subscriber is installed, so the crate's
//! log calls cost nothing.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use datadog_agent_config::apm_replace_rule::deserialize_apm_replace_rules;
use datadog_agent_config::get_config;
use serde::de::IntoDeserializer;
use serde::de::value::{Error as ValueError, StrDeserializer};

// A Lambda sandbox environment (python3.13, arm64) without any DD_* variable.
// PATH, LD_LIBRARY_PATH and TMPDIR are left as the host has them.
const LAMBDA_BASE: &[(&str, &str)] = &[
    ("AWS_LAMBDA_FUNCTION_NAME", "my-function"),
    ("AWS_LAMBDA_FUNCTION_VERSION", "$LATEST"),
    ("AWS_LAMBDA_FUNCTION_MEMORY_SIZE", "1024"),
    ("AWS_LAMBDA_LOG_GROUP_NAME", "/aws/lambda/my-function"),
    (
        "AWS_LAMBDA_LOG_STREAM_NAME",
        "2026/10/27/[$LATEST]0123456789abcdef0123456789abcdef",
    ),
    ("AWS_LAMBDA_LOG_FORMAT", "Text"),
    ("AWS_LAMBDA_RUNTIME_API", "127.0.0.1:9001"),
    ("AWS_LAMBDA_INITIALIZATION_TYPE", "on-demand"),
    ("AWS_EXECUTION_ENV", "AWS_Lambda_python3.13"),
    ("AWS_REGION", "us-east-1"),
    ("AWS_DEFAULT_REGION", "us-east-1"),
    ("AWS_ACCESS_KEY_ID", "ASIAEXAMPLEEXAMPLE12"),
    (
        "AWS_SECRET_ACCESS_KEY",
        "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
    ),
    (
        "AWS_SESSION_TOKEN",
        "IQoJb3JpZ2luX2VjEJr//////////wEaCXVzLWVhc3QtMSJHMEUCIQDexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexampleexample",
    ),
    ("AWS_XRAY_DAEMON_ADDRESS", "169.254.79.129:2000"),
    ("AWS_XRAY_CONTEXT_MISSING", "LOG_ERROR"),
    ("_AWS_XRAY_DAEMON_ADDRESS", "169.254.79.129"),
    ("_AWS_XRAY_DAEMON_PORT", "2000"),
    (
        "_X_AMZN_TRACE_ID",
        "Root=1-6720abcd-0123456789abcdef01234567;Parent=0123456789abcdef;Sampled=0;Lineage=1:abcd1234:0",
    ),
    ("LAMBDA_TASK_ROOT", "/var/task"),
    ("LAMBDA_RUNTIME_DIR", "/var/runtime"),
    ("LANG", "en_US.UTF-8"),
    ("TZ", ":UTC"),
    ("_HANDLER", "handler.handler"),
    ("PYTHONPATH", "/var/runtime"),
    ("AWS_LAMBDA_EXEC_WRAPPER", "/opt/datadog_wrapper"),
    ("SHLVL", "0"),
];

// A typical Datadog Lambda setup, as the Serverless Framework / CDK plugins generate it.
const TYPICAL: &[(&str, &str)] = &[
    (
        "DD_API_KEY_SECRET_ARN",
        "arn:aws:secretsmanager:us-east-1:123456789012:secret:dd-api-key-AbCdEf",
    ),
    ("DD_SITE", "datadoghq.com"),
    ("DD_ENV", "prod"),
    ("DD_SERVICE", "checkout"),
    ("DD_VERSION", "1.10.3"),
    ("DD_TAGS", "team:payments,tier:backend"),
    ("DD_LOG_LEVEL", "debug"),
    ("DD_TRACE_ENABLED", "true"),
    ("DD_SERVERLESS_LOGS_ENABLED", "true"),
    ("DD_LAMBDA_HANDLER", "app.handler"),
    ("DD_FLUSH_TO_LOG", "false"),
    ("DD_MERGE_XRAY_TRACES", "false"),
    ("DD_CAPTURE_LAMBDA_PAYLOAD", "false"),
    ("DD_ENHANCED_METRICS", "true"),
    ("DD_TRACE_OTEL_ENABLED", "false"),
    ("DD_LOGS_INJECTION", "true"),
    ("DD_SERVERLESS_FLUSH_STRATEGY", "end"),
    ("DD_EXTENSION_VERSION", "next"),
    ("DD_PROFILING_ENABLED", "false"),
    ("DD_SERVERLESS_APPSEC_ENABLED", "false"),
];

const ENDPOINTS_JSON: &str = r#"{"https://app.datadoghq.eu":["0123456789abcdef0123456789abcde1","0123456789abcdef0123456789abcde2"],"https://app.us5.datadoghq.com":["0123456789abcdef0123456789abcde3"]}"#;

const REPLACE_RULES_1: &str = r#"[{"name":"http.url","pattern":"(token=)[^&]*","repl":"${1}?"}]"#;
const REPLACE_RULES_3: &str = r#"[{"name":"http.url","pattern":"(token=)[^&]*","repl":"${1}?"},{"name":"resource.name","pattern":"/users/\\d+","repl":"/users/?"},{"name":"*","pattern":"\\d{16}","repl":"?"}]"#;

// A heavy setup: processing rules, dual shipping, APM replace rules, filters, OTLP, DSM.
const HEAVY_EXTRA: &[(&str, &str)] = &[
    (
        "DD_LOGS_CONFIG_PROCESSING_RULES",
        r#"[{"type":"exclude_at_match","name":"exclude_health","pattern":"GET /health"},{"type":"mask_sequences","name":"mask_cc","pattern":"\\d{4}-\\d{4}-\\d{4}-\\d{4}","replace_placeholder":"[REDACTED]"},{"type":"include_at_match","name":"inc","pattern":"ERROR|WARN"}]"#,
    ),
    ("DD_ADDITIONAL_ENDPOINTS", ENDPOINTS_JSON),
    ("DD_APM_ADDITIONAL_ENDPOINTS", ENDPOINTS_JSON),
    (
        "DD_LOGS_CONFIG_ADDITIONAL_ENDPOINTS",
        r#"[{"api_key":"0123456789abcdef0123456789abcde4","Host":"agent-http-intake.logs.datadoghq.eu","Port":443,"is_reliable":true}]"#,
    ),
    ("DD_APM_REPLACE_TAGS", REPLACE_RULES_3),
    (
        "DD_SERVICE_MAPPING",
        "lambda_sqs:my-queue,lambda_sns:my-topic",
    ),
    ("DD_APM_FILTER_TAGS_REQUIRE", "env:prod"),
    ("DD_APM_FILTER_TAGS_REJECT", "debug:true"),
    ("DD_TRACE_PROPAGATION_STYLE", "datadog,tracecontext,b3multi"),
    ("DD_PROXY_HTTPS", "http://proxy.internal:3128"),
    ("DD_PROXY_NO_PROXY", "169.254.169.254,localhost"),
    (
        "DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_ENDPOINT",
        "localhost:4318",
    ),
    ("DD_OTLP_CONFIG_TRACES_SPAN_NAME_REMAPPINGS", "a:b,c:d"),
    ("DD_LAMBDA_CUSTOMER_METRICS_EXCLUDE_TAGS", "requestid,host"),
    ("DD_SPAN_DEDUP_TIMEOUT", "5"),
    ("DD_APPSEC_WAF_TIMEOUT", "5000"),
    ("DD_DATA_STREAMS_ENABLED", "true"),
    ("DD_COMPRESSION_LEVEL", "6"),
    ("DD_FLUSH_TIMEOUT", "10"),
    ("DD_LOGS_CONFIG_USE_COMPRESSION", "true"),
    ("DD_APM_FEATURES", "enable_stats_by_span_kind"),
    ("DD_LAMBDA_EXTENSION_COMPUTE_STATS", "true"),
    ("DD_TRACE_STATS_ADDITIONAL_TAGS", "http.status_code"),
    ("DD_TRACE_EXPERIMENTAL_FEATURES_ENABLED", "true"),
    ("DD_API_KEY_SECRET_RELOAD_INTERVAL", "3600"),
    ("DD_LAMBDA_DURABLE_FUNCTION_LOG_BUFFER_SIZE", "10"),
];

const TYPICAL_YAML: &str = r#"api_key: 0123456789abcdef0123456789abcdef
site: datadoghq.eu
log_level: info
env: staging
service: yaml-svc
version: 2.3.4
tags:
  - team:yaml
  - tier:web
logs_enabled: true
logs_config:
  processing_rules:
    - type: exclude_at_match
      name: exclude_health
      pattern: "GET /health"
  use_compression: true
  compression_level: 6
apm_config:
  replace_tags:
    - name: http.url
      pattern: "(token=)[^&]*"
      repl: "${1}?"
  features:
    - enable_stats_by_span_kind
  obfuscation:
    http:
      remove_query_string: true
otlp_config:
  receiver:
    protocols:
      http:
        endpoint: localhost:4318
serverless_flush_strategy: end
enhanced_metrics: true
capture_lambda_payload: true
api_key_secret_arn: arn:aws:secretsmanager:us-east-1:123456789012:secret:yaml
"#;

fn set_all(pairs: &[(&str, &str)]) {
    for (k, v) in pairs {
        // SAFETY: criterion runs benches on this thread and no other thread is
        // alive that reads the environment.
        unsafe { std::env::set_var(k, v) }
    }
}

fn remove_dd_env() {
    let keys: Vec<_> = std::env::vars_os()
        .map(|(k, _)| k)
        .filter(|k| {
            let k = k.to_string_lossy();
            // `get_config` also reads the proxy variables.
            k.starts_with("DD_")
                || k.starts_with("DATADOG_")
                || k.eq_ignore_ascii_case("HTTPS_PROXY")
                || k.eq_ignore_ascii_case("NO_PROXY")
        })
        .collect();
    for k in keys {
        // SAFETY: see `set_all`.
        unsafe { std::env::remove_var(k) }
    }
}

fn set_scenario(scenario: &str) {
    remove_dd_env();
    set_all(LAMBDA_BASE);
    match scenario {
        "none" => {}
        "typical" => set_all(TYPICAL),
        "heavy" => {
            set_all(TYPICAL);
            set_all(HEAVY_EXTRA);
        }
        other => panic!("unknown scenario {other}"),
    }
}

fn bench_get_config(c: &mut Criterion) {
    let empty_dir = tempfile::tempdir().expect("tempdir");
    let yaml_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(yaml_dir.path().join("datadog.yaml"), TYPICAL_YAML).expect("write yaml");

    let mut group = c.benchmark_group("config");
    for scenario in ["none", "typical", "heavy"] {
        set_scenario(scenario);
        let config = get_config(empty_dir.path());
        assert_eq!(config.service.is_some(), scenario != "none");
        assert_eq!(
            config.apm_replace_tags.map_or(0, |r| r.len()),
            if scenario == "heavy" { 3 } else { 0 }
        );
        group.bench_function(BenchmarkId::new("get_config", scenario), |b| {
            b.iter(|| get_config(black_box(empty_dir.path())))
        });
    }
    set_scenario("typical");
    let config = get_config(yaml_dir.path());
    assert_eq!(config.api_key, "0123456789abcdef0123456789abcdef");
    assert_eq!(config.service.as_deref(), Some("checkout"));
    group.bench_function(BenchmarkId::new("get_config_yaml", "typical"), |b| {
        b.iter(|| get_config(black_box(yaml_dir.path())))
    });
    remove_dd_env();
    group.finish();
}

fn bench_replace_rules(c: &mut Criterion) {
    // The deserializer both config sources use, given the JSON string form.
    let parse = |json: &str| {
        let de: StrDeserializer<'_, ValueError> = json.into_deserializer();
        deserialize_apm_replace_rules(de)
    };
    let mut group = c.benchmark_group("config");
    for (count, json) in [(1, REPLACE_RULES_1), (3, REPLACE_RULES_3)] {
        let rules = parse(json).expect("deserialize").expect("rules");
        assert_eq!(rules.len(), count);
        group.throughput(Throughput::Elements(count as u64));
        group.bench_function(BenchmarkId::new("replace_rules", count), |b| {
            b.iter(|| parse(black_box(json)))
        });
    }
    group.finish();
}

criterion_group!(benches, bench_get_config, bench_replace_rules);
criterion_main!(benches);
