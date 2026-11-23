// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn datadog_init() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_datadog-init"));
    command.env_remove("DD_API_KEY");
    command
}

fn send_signal(child: &Child, signal: libc::c_int) {
    let pid = libc::pid_t::try_from(child.id()).expect("process ID fits in pid_t");
    // SAFETY: kill has no memory safety requirements.
    assert_eq!(unsafe { libc::kill(pid, signal) }, 0, "kill failed");
}

fn wait_with_timeout(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait failed") {
            return status;
        }
        if Instant::now() > deadline {
            child.kill().expect("kill failed");
            panic!("datadog-init did not exit within 10 seconds");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The fixture child traps SIGTERM, prints a marker and exits 7.
#[test]
fn forwards_sigterm_to_child_and_exits_with_its_code() {
    let mut child = datadog_init()
        .args([
            "sh",
            "-c",
            "trap 'echo child got SIGTERM; exit 7' TERM; echo child ready; while :; do sleep 0.05; done",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to start datadog-init");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
    let mut line = String::new();
    stdout.read_line(&mut line).expect("read failed");
    assert_eq!(line, "child ready\n");

    send_signal(&child, libc::SIGTERM);

    assert_eq!(wait_with_timeout(&mut child).code(), Some(7));
    line.clear();
    stdout.read_line(&mut line).expect("read failed");
    assert_eq!(line, "child got SIGTERM\n");
}

/// One argument is split on whitespace, the child inherits the in-container defaults without
/// overriding a value that is already set, and its stdout and stderr pass through.
#[test]
fn splits_single_argument_and_passes_environment_and_output() {
    let output = datadog_init()
        .arg("sh -c env;echo>&2")
        .env("DD_APM_ENABLED", "false")
        .env_remove("DD_HOSTNAME")
        .env_remove("DD_LOGS_ENABLED")
        .output()
        .expect("failed to run datadog-init");

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let environment: Vec<&str> = stdout.lines().collect();
    for expected in [
        "DD_HOSTNAME=none",
        "DD_APM_ENABLED=false",
        "DD_TRACE_ENABLED=true",
    ] {
        assert!(
            environment.contains(&expected),
            "missing {expected} in {stdout}"
        );
    }
    assert!(
        !stdout.contains("DD_LOGS_ENABLED="),
        "unexpected sidecar default in {stdout}"
    );
    assert_eq!(output.stderr, b"\n");
}

#[test]
fn exits_1_when_child_cannot_start() {
    let status = datadog_init()
        .arg("/nonexistent/datadog-init-test-binary")
        .stderr(Stdio::null())
        .status()
        .expect("failed to run datadog-init");
    assert_eq!(status.code(), Some(1));
}

/// On Azure Container Apps with an API key and no subscription, Go exits 1 before the child starts.
#[test]
fn container_app_without_subscription_exits_1_before_the_child() {
    let marker = std::env::temp_dir().join(format!("datadog-init-child-{}", std::process::id()));
    let output = datadog_init()
        .env("CONTAINER_APP_NAME", "app")
        .env("DD_API_KEY", "test-api-key")
        .env_remove("DD_AZURE_SUBSCRIPTION_ID")
        .env("DD_AZURE_RESOURCE_GROUP", "group")
        .env("DD_LOG_LEVEL", "off")
        .args(["touch", marker.to_str().unwrap()])
        .output()
        .expect("failed to run datadog-init");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Must set Subscription ID as an environment variable."),
        "stderr {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!marker.exists(), "the child ran");
}

#[test]
fn exits_255_when_child_is_killed_by_signal() {
    let status = datadog_init()
        .args(["sh", "-c", "kill -KILL $$"])
        .status()
        .expect("failed to run datadog-init");
    assert_eq!(status.code(), Some(255));
}

#[test]
fn sidecar_exits_0_on_sigterm() {
    let mut child = datadog_init()
        .env("DD_LOG_LEVEL", "info")
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to start datadog-init");
    // The sidecar logs that it waits only after it registers its handlers.
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
    let mut line = String::new();
    while !line.contains("waiting for SIGINT or SIGTERM") {
        line.clear();
        assert_ne!(
            stdout.read_line(&mut line).expect("read failed"),
            0,
            "stdout closed"
        );
    }

    send_signal(&child, libc::SIGTERM);

    assert_eq!(wait_with_timeout(&mut child).code(), Some(0));
}

/// A request that reached the logs intake: its path, its lowercased headers and its body.
struct LogsRequest {
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

/// Accepts HTTP requests on a free local port, answers 202, and returns the port and a channel
/// of the requests.
fn start_logs_intake() -> (u16, std::sync::mpsc::Receiver<LogsRequest>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind failed");
    let port = listener.local_addr().expect("local address").port();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let mut reader = BufReader::new(stream);
            let mut request_line = String::new();
            reader.read_line(&mut request_line).expect("request line");
            let mut headers = BTreeMap::new();
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|read| read > 2) {
                if let Some((name, value)) = line.split_once(':') {
                    headers.insert(name.to_ascii_lowercase(), value.trim().to_string());
                }
                line.clear();
            }
            let content_length = headers
                .get("content-length")
                .map_or(0, |value| value.parse().expect("content length"));
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).expect("read body");
            let _ = reader
                .get_mut()
                .write_all(b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\r\n");
            let path = request_line
                .split(' ')
                .nth(1)
                .unwrap_or_default()
                .to_string();
            let _ = sender.send(LogsRequest {
                path,
                headers,
                body,
            });
        }
    });
    (port, receiver)
}

/// With logs on, the child's output still passes through unchanged, and reaches the logs intake
/// at shutdown: stdout one message per line, stderr one message per write.
#[test]
fn sends_child_output_as_logs_and_passes_it_through() {
    let (port, bodies) = start_logs_intake();
    let output = datadog_init()
        .args(["sh", "-c", "printf 'one\\ntwo\\n'; printf 'a\\nb\\n' >&2"])
        .env("DD_LOGS_ENABLED", "true")
        .env("DD_LOGS_CONFIG_LOGS_DD_URL", format!("127.0.0.1:{port}"))
        .env("DD_LOGS_CONFIG_LOGS_NO_SSL", "true")
        .env("DD_SERVICE", "parity-app")
        .env("DD_LOG_LEVEL", "off")
        .output()
        .expect("failed to run datadog-init");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"one\ntwo\n");
    assert_eq!(output.stderr, b"a\nb\n");
    let request = bodies
        .recv_timeout(Duration::from_secs(5))
        .expect("no logs reached the intake");
    assert_eq!(request.path, "/api/v2/logs");
    let header = |name: &str| request.headers.get(name).map(String::as_str);
    assert_eq!(header("dd-protocol"), None);
    assert_eq!(header("user-agent"), Some("datadog-agent/0.1.0"));
    assert!(header("dd-message-timestamp").is_some_and(|value| value.parse::<u64>().is_ok()));
    assert!(header("dd-current-timestamp").is_some_and(|value| value.parse::<u64>().is_ok()));
    assert_eq!(header("content-type"), Some("application/json"));
    assert_eq!(header("content-encoding"), Some("zstd"));
    assert_eq!(header("dd-evp-origin"), Some("serverless"));
    assert_eq!(header("dd-evp-origin-version"), Some("0.1.0"));
    let body = zstd::stream::decode_all(request.body.as_slice()).expect("zstd body");
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&body).expect("JSON array");
    let mut messages: Vec<(String, String, String)> = entries
        .iter()
        .map(|entry| {
            (
                entry["message"].as_str().expect("message").to_string(),
                entry["status"].as_str().expect("status").to_string(),
                entry["service"].as_str().expect("service").to_string(),
            )
        })
        .collect();
    messages.sort();
    let expected: Vec<(String, String, String)> =
        [("a\nb\n", "error"), ("one", "info"), ("two", "info")]
            .into_iter()
            .map(|(message, status)| (message.into(), status.into(), "parity-app".into()))
            .collect();
    assert_eq!(messages, expected);
}
