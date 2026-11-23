// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! The logs agent: sends the child's stdout and stderr, and the lines appended to the files that
//! `DD_SERVERLESS_LOG_PATH` names, to the logs intake, as Go serverless-init does when
//! `DD_LOGS_ENABLED` is true.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;

use datadog_logs_agent::{
    AggregatorHandle, AggregatorService, CustomUrlHeaders, Destination, IntakeEntry, IntakeOrigin,
    LogFlusher, LogFlusherConfig,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

use crate::file_tailer::{self, FileTailer};
use crate::metrics_agent::build_client;
use crate::settings;

/// How often buffered logs are sent while the child runs: the Go logs agent's default batch
/// wait.
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// Whether logs are on: `DD_LOGS_ENABLED` is `true`, in any case. Go serverless-init sends logs
/// without an API key too.
pub fn enabled(lookup: impl Fn(&str) -> Option<String>) -> bool {
    lookup("DD_LOGS_ENABLED").is_some_and(|value| value.eq_ignore_ascii_case("true"))
}

/// The logs intake URL, with Go's precedence: an observability pipelines worker that is enabled
/// and has a URL, then `DD_LOGS_CONFIG_LOGS_DD_URL`, then the site's intake. An address that Go
/// cannot parse is an error, as Go then starts no logs agent.
fn intake_url(lookup: impl Fn(&str) -> Option<String>) -> Result<String, String> {
    let no_ssl = lookup("DD_LOGS_CONFIG_LOGS_NO_SSL")
        .is_some_and(|value| value.eq_ignore_ascii_case("true"));
    let worker_enabled = lookup("DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_ENABLED")
        .is_some_and(|value| value.eq_ignore_ascii_case("true"));
    let worker_url =
        lookup("DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_URL").filter(|url| !url.trim().is_empty());
    if let (true, Some(url)) = (worker_enabled, worker_url) {
        return custom_url(url.trim(), no_ssl, false);
    }
    if let Some(address) =
        lookup("DD_LOGS_CONFIG_LOGS_DD_URL").filter(|address| !address.trim().is_empty())
    {
        return custom_url(address.trim(), no_ssl, true);
    }
    Ok(format!(
        "https://http-intake.logs.{}/api/v2/logs",
        settings::intake_site(lookup)
    ))
}

/// The full intake URL for an address, as Go builds it. With a scheme, the scheme decides HTTPS
/// and `logs_no_ssl` is ignored. With `keep_path`, a path other than a legacy intake path
/// prefixes `/api/v2/logs`; otherwise the path is dropped. Without a scheme, the address must be
/// `host:port`.
fn custom_url(address: &str, no_ssl: bool, keep_path: bool) -> Result<String, String> {
    let scheme_and_rest = address
        .strip_prefix("https://")
        .map(|rest| ("https", rest))
        .or_else(|| address.strip_prefix("http://").map(|rest| ("http", rest)));
    if let Some((scheme, rest)) = scheme_and_rest {
        let rest = rest.split(['?', '#']).next().unwrap_or_default();
        let (host, path) = rest
            .find('/')
            .map_or((rest, ""), |slash| rest.split_at(slash));
        let path = if !keep_path || ["/v1/input", "/api/v2/logs"].contains(&path) {
            ""
        } else {
            path
        };
        return Ok(format!("{scheme}://{host}{path}/api/v2/logs"));
    }
    let has_port = address
        .rsplit_once(':')
        .is_some_and(|(_, port)| port.parse::<u16>().is_ok());
    if !has_port {
        return Err(format!(
            "could not parse {address}: missing port in address"
        ));
    }
    let scheme = if no_ssl { "http" } else { "https" };
    Ok(format!("{scheme}://{address}/api/v2/logs"))
}

/// The headers that Go serverless-init sends to every logs intake. It sends no `DD-PROTOCOL`.
fn serverless_headers() -> CustomUrlHeaders {
    CustomUrlHeaders {
        protocol: None,
        origin: Some(IntakeOrigin {
            name: "serverless".to_string(),
            version: crate::platform::VERSION.to_string(),
        }),
        user_agent: Some(format!("datadog-agent/{}", crate::platform::VERSION)),
        timestamps: true,
    }
}

/// The zstd level: `DD_LOGS_CONFIG_ZSTD_COMPRESSION_LEVEL`, else Go's default of 1.
/// `DD_LOGS_CONFIG_COMPRESSION_LEVEL` is gzip's level in Go, so it is not read.
fn zstd_compression_level(lookup: impl Fn(&str) -> Option<String>) -> i32 {
    lookup("DD_LOGS_CONFIG_ZSTD_COMPRESSION_LEVEL")
        .and_then(|level| level.trim().parse().ok())
        .unwrap_or(1)
}

/// The service of logs, lowercased: the first non-empty of `DD_SERVICE`, `K_SERVICE` and
/// `CONTAINER_APP_NAME`, else `agent`, as Go's channel tailer finds it. `DD_TAGS` is not read.
fn service(lookup: impl Fn(&str) -> Option<String>) -> String {
    ["DD_SERVICE", "K_SERVICE", "CONTAINER_APP_NAME"]
        .into_iter()
        .filter_map(lookup)
        .find(|service| !service.is_empty())
        .map_or_else(|| "agent".to_string(), |service| service.to_lowercase())
}

/// Where logs come from, which sets their service and their tags.
#[derive(Debug)]
enum Origin<'a> {
    /// The child's stdout and stderr.
    Output,
    /// A tailed file.
    File(&'a Path),
}

/// The template of every entry of `origin`: the service, the source and the tags. The hostname is
/// empty, as Go serverless-init sends it. `DD_SOURCE` sets the source, lowercased; it defaults to
/// the platform's `default_source`. A file's logs have the service `DD_SERVICE`
/// only, as it is, and the tags `filename` and `dirname`, as Go's file source sets them.
fn template(
    tags: &BTreeMap<String, String>,
    default_source: &str,
    lookup: impl Fn(&str) -> Option<String>,
    origin: &Origin,
) -> IntakeEntry {
    let mut template = IntakeEntry::from_message(String::new(), 0);
    template.hostname = Some(String::new());
    template.ddsource = Some(
        lookup("DD_SOURCE")
            .map(|source| source.to_lowercase())
            .filter(|source| !source.is_empty())
            .unwrap_or_else(|| default_source.to_string()),
    );
    match origin {
        Origin::Output => {
            template.service = Some(service(&lookup));
            template.ddtags = Some(crate::platform::join_tags(tags));
        }
        Origin::File(path) => {
            template.service = lookup("DD_SERVICE").filter(|service| !service.is_empty());
            let mut tags = tags.clone();
            let name = path.file_name().unwrap_or_default();
            let directory = path
                .parent()
                .filter(|directory| !directory.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            tags.insert("filename".to_string(), name.to_string_lossy().into_owned());
            tags.insert("dirname".to_string(), directory.display().to_string());
            template.ddtags = Some(crate::platform::join_tags(&tags));
        }
    }
    template
}

/// Turns the child's output or a file's lines into log entries. Cloning shares the same
/// aggregator.
#[derive(Clone)]
pub struct LogSink {
    aggregator: AggregatorHandle,
    template: IntakeEntry,
}

impl LogSink {
    /// Sends one message: `info` for stdout, `error` for stderr.
    pub fn send(&self, message: &[u8], is_error: bool) {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or_default();
        let entry = entry(&self.template, message, is_error, timestamp);
        if let Err(error) = self.aggregator.insert_batch(vec![entry]) {
            debug!("Log dropped: {error}");
        }
    }
}

fn entry(template: &IntakeEntry, message: &[u8], is_error: bool, timestamp: i64) -> IntakeEntry {
    IntakeEntry {
        message: String::from_utf8_lossy(message).into_owned(),
        timestamp,
        status: Some(if is_error { "error" } else { "info" }.to_string()),
        ..template.clone()
    }
}

/// Splits stdout into lines, as Go serverless-init does: one message per line without its
/// newline, and an unfinished line waits for the rest.
#[derive(Default)]
pub struct LineSplitter {
    pending: Vec<u8>,
}

/// The newline positions of `bytes`.
fn newlines(bytes: &[u8]) -> impl Iterator<Item = usize> + '_ {
    bytes
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'\n')
        .map(|(position, _)| position)
}

impl LineSplitter {
    /// Adds `chunk` and returns the lines it completes. Only `chunk` is scanned, so a long
    /// line costs time in proportion to its length.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        let mut lines = Vec::new();
        let mut start = 0;
        for end in newlines(chunk) {
            let mut line = std::mem::take(&mut self.pending);
            line.extend_from_slice(&chunk[start..end]);
            lines.push(line);
            start = end + 1;
        }
        self.pending.extend_from_slice(&chunk[start..]);
        lines
    }
}

/// The running logs agent, kept until shutdown.
pub struct LogsAgent {
    sink: LogSink,
    flusher: LogFlusher,
    cancel_token: CancellationToken,
}

impl LogsAgent {
    /// Starts the aggregator and the periodic flush when logs are on. Returns `None`, after
    /// logging why, when they are off or the HTTP client cannot be built. `default_source` is
    /// the source when `DD_SOURCE` is not set.
    pub fn start(tags: &BTreeMap<String, String>, default_source: &'static str) -> Option<Self> {
        let lookup = |name: &str| std::env::var(name).ok();
        if !enabled(lookup) {
            debug!("Logs are disabled");
            return None;
        }
        let client = match build_client(settings::proxy_url(lookup)) {
            Ok(client) => client,
            Err(error) => {
                error!("Failed to build the logs HTTP client: {error}; logs are disabled");
                return None;
            }
        };
        let mut config = LogFlusherConfig {
            api_key: settings::api_key(lookup).unwrap_or_default(),
            site: settings::site(lookup),
            ..LogFlusherConfig::from_env()
        };
        let url = match intake_url(lookup) {
            Ok(url) => url,
            Err(error) => {
                error!("Invalid logs intake address: {error}; logs are disabled");
                return None;
            }
        };
        config.mode = Destination::CustomUrl {
            url,
            headers: serverless_headers(),
        };
        config.compression_level = zstd_compression_level(lookup);
        let (service, aggregator) = AggregatorService::new();
        tokio::spawn(service.run());
        let flusher = LogFlusher::new(config, client, aggregator.clone());
        let cancel_token = CancellationToken::new();
        tokio::spawn(flush_periodically(flusher.clone(), cancel_token.clone()));
        if let Some(pattern) = lookup("DD_SERVERLESS_LOG_PATH") {
            tokio::spawn(tail_files(
                pattern,
                aggregator.clone(),
                tags.clone(),
                default_source,
                cancel_token.clone(),
            ));
        }
        Some(Self {
            sink: LogSink {
                template: template(tags, default_source, lookup, &Origin::Output),
                aggregator,
            },
            flusher,
            cancel_token,
        })
    }

    pub fn sink(&self) -> LogSink {
        self.sink.clone()
    }

    /// Stops the periodic flush and sends every buffered log.
    pub async fn flush_at_shutdown(self) {
        self.cancel_token.cancel();
        self.flusher.flush(Vec::new()).await;
        let _ = self.sink.aggregator.shutdown();
    }
}

/// Sends the lines appended to the files that `pattern` matches, until shutdown. As in Go, the
/// lines not read yet at shutdown are not sent.
async fn tail_files(
    pattern: String,
    aggregator: AggregatorHandle,
    tags: BTreeMap<String, String>,
    default_source: &'static str,
    cancel_token: CancellationToken,
) {
    let lookup = |name: &str| std::env::var(name).ok();
    let mut tailer = FileTailer::new(pattern);
    let mut sinks: HashMap<std::path::PathBuf, LogSink> = HashMap::new();
    let mut interval = tokio::time::interval(file_tailer::POLL_INTERVAL);
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => return,
            _ = interval.tick() => {}
        }
        let Some((polled, lines)) = tailer.poll_in_background().await else {
            error!("The log file tailer failed; file tailing stopped");
            return;
        };
        tailer = polled;
        for (path, line) in lines {
            sinks
                .entry(path)
                .or_insert_with_key(|path| LogSink {
                    aggregator: aggregator.clone(),
                    template: template(&tags, default_source, lookup, &Origin::File(path)),
                })
                .send(&line, false);
        }
    }
}

async fn flush_periodically(flusher: LogFlusher, cancel_token: CancellationToken) {
    let mut interval = tokio::time::interval(FLUSH_INTERVAL);
    interval.tick().await;
    let mut retry = Vec::new();
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => return,
            _ = interval.tick() => retry = flusher.flush(retry).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Variables = &'static [(&'static str, &'static str)];

    fn lookup_in<'a>(variables: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'a {
        |name| {
            variables
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    #[test]
    fn enabled_table() {
        let cases: &[(Variables, bool)] = &[
            (&[], false),
            (&[("DD_LOGS_ENABLED", "true")], true),
            (&[("DD_LOGS_ENABLED", "TRUE")], true),
            (&[("DD_LOGS_ENABLED", "1")], false),
            (&[("DD_LOGS_ENABLED", "false")], false),
        ];
        for (variables, expected) in cases {
            assert_eq!(enabled(lookup_in(variables)), *expected, "{variables:?}");
        }
    }

    #[test]
    fn intake_url_table() {
        let url = |url: &str| Ok(url.to_string());
        let cases: &[(Variables, Result<String, String>)] = &[
            (
                &[],
                url("https://http-intake.logs.datadoghq.com./api/v2/logs"),
            ),
            (
                &[("DD_SITE", "datadoghq.eu")],
                url("https://http-intake.logs.datadoghq.eu./api/v2/logs"),
            ),
            (
                &[("DD_LOGS_CONFIG_LOGS_DD_URL", "")],
                url("https://http-intake.logs.datadoghq.com./api/v2/logs"),
            ),
            (
                &[("DD_LOGS_CONFIG_LOGS_DD_URL", "fakeintake:80")],
                url("https://fakeintake:80/api/v2/logs"),
            ),
            (
                &[
                    ("DD_LOGS_CONFIG_LOGS_DD_URL", "fakeintake:80"),
                    ("DD_LOGS_CONFIG_LOGS_NO_SSL", "true"),
                ],
                url("http://fakeintake:80/api/v2/logs"),
            ),
            (
                &[
                    ("DD_LOGS_CONFIG_LOGS_DD_URL", "https://proxy:8443"),
                    ("DD_LOGS_CONFIG_LOGS_NO_SSL", "true"),
                ],
                url("https://proxy:8443/api/v2/logs"),
            ),
            (
                &[("DD_LOGS_CONFIG_LOGS_DD_URL", "http://proxy/prefix")],
                url("http://proxy/prefix/api/v2/logs"),
            ),
            (
                &[("DD_LOGS_CONFIG_LOGS_DD_URL", "http://proxy:80/api/v2/logs")],
                url("http://proxy:80/api/v2/logs"),
            ),
            (
                &[("DD_LOGS_CONFIG_LOGS_DD_URL", "https://proxy/v1/input")],
                url("https://proxy/api/v2/logs"),
            ),
            (
                &[("DD_LOGS_CONFIG_LOGS_DD_URL", "fakeintake")],
                Err("could not parse fakeintake: missing port in address".to_string()),
            ),
            (
                &[
                    ("DD_LOGS_CONFIG_LOGS_DD_URL", "fakeintake:80"),
                    ("DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_ENABLED", "true"),
                    (
                        "DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_URL",
                        "http://worker:8282/ignored",
                    ),
                ],
                url("http://worker:8282/api/v2/logs"),
            ),
            (
                &[
                    ("DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_ENABLED", "true"),
                    ("DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_URL", "worker:8282"),
                ],
                url("https://worker:8282/api/v2/logs"),
            ),
            (
                &[
                    ("DD_LOGS_CONFIG_LOGS_DD_URL", "fakeintake:80"),
                    ("DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_ENABLED", "true"),
                ],
                url("https://fakeintake:80/api/v2/logs"),
            ),
            (
                &[("DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_ENABLED", "true")],
                url("https://http-intake.logs.datadoghq.com./api/v2/logs"),
            ),
            (
                &[
                    ("DD_LOGS_CONFIG_LOGS_DD_URL", "fakeintake:80"),
                    (
                        "DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_URL",
                        "http://worker:8282",
                    ),
                ],
                url("https://fakeintake:80/api/v2/logs"),
            ),
        ];
        for (variables, expected) in cases {
            assert_eq!(&intake_url(lookup_in(variables)), expected, "{variables:?}");
        }
    }

    #[test]
    fn zstd_compression_level_table() {
        let cases: &[(Variables, i32)] = &[
            (&[], 1),
            (&[("DD_LOGS_CONFIG_ZSTD_COMPRESSION_LEVEL", "5")], 5),
            (&[("DD_LOGS_CONFIG_ZSTD_COMPRESSION_LEVEL", "fast")], 1),
            (&[("DD_LOGS_CONFIG_COMPRESSION_LEVEL", "9")], 1),
        ];
        for (variables, expected) in cases {
            assert_eq!(
                zstd_compression_level(lookup_in(variables)),
                *expected,
                "{variables:?}"
            );
        }
    }

    #[test]
    fn line_splitter_table() {
        type Chunks = &'static [&'static [u8]];
        let cases: &[(Chunks, Chunks)] = &[
            (&[b"one\ntwo\n"], &[b"one", b"two"]),
            (&[b"par", b"tial\n"], &[b"partial"]),
            (&[b"unfinished"], &[]),
            (&[b"\n"], &[b""]),
        ];
        for (chunks, expected) in cases {
            let mut splitter = LineSplitter::default();
            let lines: Vec<Vec<u8>> = chunks
                .iter()
                .flat_map(|chunk| splitter.push(chunk))
                .collect();
            assert_eq!(lines, *expected, "{chunks:?}");
        }
    }

    #[test]
    fn service_table() {
        let cases: &[(Variables, &str)] = &[
            (&[], "agent"),
            (&[("DD_TAGS", "service:x")], "agent"),
            (&[("CONTAINER_APP_NAME", "App")], "app"),
            (
                &[("K_SERVICE", "Run"), ("CONTAINER_APP_NAME", "app")],
                "run",
            ),
            (
                &[
                    ("DD_SERVICE", "Api"),
                    ("K_SERVICE", "run"),
                    ("DD_TAGS", "service:x"),
                ],
                "api",
            ),
            (&[("DD_SERVICE", ""), ("K_SERVICE", "run")], "run"),
        ];
        for (variables, expected) in cases {
            assert_eq!(service(lookup_in(variables)), *expected, "{variables:?}");
        }
    }

    /// A line of many reads is joined once, without scanning the bytes held so far again.
    #[test]
    fn line_splitter_joins_long_line_of_many_reads() {
        let mut splitter = LineSplitter::default();
        let chunk = vec![b'x'; 32 * 1024];
        for _ in 0..1024 {
            assert!(splitter.push(&chunk).is_empty());
        }
        let lines = splitter.push(b"\n");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), 32 * 1024 * 1024);
    }

    #[test]
    fn sink_entry_table() {
        let tags: BTreeMap<String, String> = [("local", "true"), ("service", "parity-app")]
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        let file = Path::new("/shared/app.log");
        type Case<'a> = (
            &'a BTreeMap<String, String>,
            Variables,
            Origin<'a>,
            bool,
            &'a str,
        );
        let cases: &[Case] = &[
            (
                &tags,
                &[("DD_SERVICE", "parity-app")],
                Origin::Output,
                false,
                r#"{"message":"hello","timestamp":42,"hostname":"","service":"parity-app","ddsource":"unknown","ddtags":"local:true,service:parity-app","status":"info"}"#,
            ),
            (
                &BTreeMap::new(),
                &[("DD_SOURCE", "Python")],
                Origin::Output,
                true,
                r#"{"message":"hello","timestamp":42,"hostname":"","service":"agent","ddsource":"python","ddtags":"","status":"error"}"#,
            ),
            (
                &tags,
                &[("K_SERVICE", "run")],
                Origin::File(file),
                false,
                r#"{"message":"hello","timestamp":42,"hostname":"","ddsource":"unknown","ddtags":"dirname:/shared,filename:app.log,local:true,service:parity-app","status":"info"}"#,
            ),
            (
                &BTreeMap::new(),
                &[],
                Origin::File(Path::new("app.log")),
                false,
                r#"{"message":"hello","timestamp":42,"hostname":"","ddsource":"unknown","ddtags":"dirname:.,filename:app.log","status":"info"}"#,
            ),
            (
                &BTreeMap::new(),
                &[("DD_SERVICE", "Api")],
                Origin::File(file),
                false,
                r#"{"message":"hello","timestamp":42,"hostname":"","service":"Api","ddsource":"unknown","ddtags":"dirname:/shared,filename:app.log","status":"info"}"#,
            ),
        ];
        for (tags, variables, origin, is_error, expected) in cases {
            let template = template(tags, "unknown", lookup_in(variables), origin);
            let json = serde_json::to_string(&entry(&template, b"hello", *is_error, 42))
                .expect("serialize");
            assert_eq!(json, *expected, "{variables:?} {origin:?}");
        }
    }
}
