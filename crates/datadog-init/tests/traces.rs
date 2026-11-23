// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use datadog_mock_intake::MockIntake;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind failed")
        .local_addr()
        .expect("local_addr failed")
        .port()
}

fn v04_payload() -> Vec<u8> {
    let span = serde_json::json!({
        "service": "parity-app",
        "name": "parity.request",
        "resource": "GET /",
        "trace_id": 11u64,
        "span_id": 22u64,
        "parent_id": 0u64,
        "start": 1_700_000_000_000_000_000i64,
        "duration": 50_000_000i64,
        "error": 0,
        "meta": {"span.kind": "server"},
        "metrics": {},
        "type": "web",
    });
    rmp_serde::to_vec_named(&vec![vec![span]]).expect("serialize failed")
}

/// Starts `datadog-init` with `arguments`, sending traces to `trace_intake_url` through a
/// receiver on `receiver_port`. DogStatsD is off, so only the trace agent flushes.
fn start_datadog_init(arguments: &[&str], trace_intake_url: &str, receiver_port: u16) -> Child {
    Command::new(env!("CARGO_BIN_EXE_datadog-init"))
        .args(arguments)
        .env("DD_API_KEY", "test-api-key")
        .env("DD_APM_DD_URL", trace_intake_url)
        .env("DD_APM_RECEIVER_PORT", receiver_port.to_string())
        .env("DD_USE_DOGSTATSD", "false")
        .env("DD_LOG_LEVEL", "info")
        // A configured install signature keeps the agent from writing `install.json` here.
        .env("DD_INSTRUMENTATION_INSTALL_ID", "test-install")
        .env_remove("DD_SITE")
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to start datadog-init")
}

/// Reads stdout lines until one contains `marker`.
fn wait_for_line(child: &mut Child, marker: &str) -> BufReader<std::process::ChildStdout> {
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
    let mut line = String::new();
    while !line.contains(marker) {
        line.clear();
        assert_ne!(
            stdout.read_line(&mut line).expect("read failed"),
            0,
            "stdout closed before {marker:?}"
        );
    }
    stdout
}

/// Sends one `/v0.4` trace, retrying until the receiver answers.
async fn send_trace(receiver_port: u16) {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    let response = loop {
        let result = client
            .post(format!("http://127.0.0.1:{receiver_port}/v0.4/traces"))
            .header("Content-Type", "application/msgpack")
            .body(v04_payload())
            .send()
            .await;
        match result {
            Ok(response) => break response,
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await
            }
            Err(error) => panic!("receiver never answered: {error}"),
        }
    };
    assert!(
        response.status().is_success(),
        "status {}",
        response.status()
    );
}

fn send_signal(child: &Child, signal: libc::c_int) {
    let pid = libc::pid_t::try_from(child.id()).expect("process ID fits in pid_t");
    // SAFETY: kill has no memory safety requirements.
    assert_eq!(unsafe { libc::kill(pid, signal) }, 0, "kill failed");
}

async fn wait(mut child: Child) -> std::process::ExitStatus {
    tokio::task::spawn_blocking(move || child.wait())
        .await
        .expect("join failed")
        .expect("wait failed")
}

fn span_resources(intake: &MockIntake) -> Vec<String> {
    intake
        .trace_payloads()
        .iter()
        .flat_map(|payload| &payload.tracer_payloads)
        .flat_map(|tracer_payload| &tracer_payload.chunks)
        .flat_map(|chunk| &chunk.spans)
        .map(|span| span.resource.clone())
        .collect()
}

/// A `/v0.4` trace sent while the child runs reaches the intake from `DD_APM_DD_URL` at
/// shutdown, before the periodic flush.
#[tokio::test(flavor = "multi_thread")]
async fn forwards_v04_trace_to_intake_at_shutdown() {
    let intake = MockIntake::start().await;
    let port = free_port();
    let child = start_datadog_init(&["sh", "-c", "sleep 1.5"], &intake.base_url(), port);

    send_trace(port).await;

    assert_eq!(wait(child).await.code(), Some(0));
    assert_eq!(span_resources(&intake), vec!["GET /".to_string()]);
}

/// A trace sent just before SIGTERM reaches the intake: the child exits on the forwarded
/// SIGTERM, and the shutdown flush sends the trace.
#[tokio::test(flavor = "multi_thread")]
async fn flushes_trace_sent_just_before_sigterm() {
    let intake = MockIntake::start().await;
    let port = free_port();
    let mut child = start_datadog_init(
        &[
            "sh",
            "-c",
            "trap 'exit 0' TERM; echo child ready; while :; do sleep 0.05; done",
        ],
        &intake.base_url(),
        port,
    );
    let _stdout = wait_for_line(&mut child, "child ready");

    send_trace(port).await;
    send_signal(&child, libc::SIGTERM);

    assert_eq!(wait(child).await.code(), Some(0));
    assert_eq!(span_resources(&intake), vec!["GET /".to_string()]);
}

/// A second SIGTERM during the shutdown flush is ignored, as in Go: the process keeps its exit
/// code, and still exits once the 2.5-second trace budget runs out. The intake accepts
/// connections but never answers, so the flush lasts the whole budget.
async fn ignores_second_sigterm_during_flush(arguments: &[&str], ready: &str, exit_code: i32) {
    let silent_intake = std::net::TcpListener::bind("127.0.0.1:0").expect("bind failed");
    let silent_intake_url = format!("http://{}", silent_intake.local_addr().unwrap());
    let port = free_port();
    let mut child = start_datadog_init(arguments, &silent_intake_url, port);
    let _stdout = wait_for_line(&mut child, ready);
    send_trace(port).await;

    send_signal(&child, libc::SIGTERM);
    let first_sigterm = Instant::now();
    tokio::time::sleep(Duration::from_millis(500)).await;
    send_signal(&child, libc::SIGTERM);
    let status = wait(child).await;

    let elapsed = first_sigterm.elapsed();
    assert_eq!(status.code(), Some(exit_code), "status {status:?}");
    assert!(
        elapsed > Duration::from_secs(2) && elapsed < Duration::from_millis(3500),
        "exited {elapsed:?} after the first SIGTERM"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ignores_second_sigterm_during_flush_in_container() {
    ignores_second_sigterm_during_flush(
        &[
            "sh",
            "-c",
            "trap 'exit 3' TERM; echo child ready; while :; do sleep 0.05; done",
        ],
        "child ready",
        3,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ignores_second_sigterm_during_flush_as_sidecar() {
    ignores_second_sigterm_during_flush(&[], "waiting for SIGINT or SIGTERM", 0).await;
}
