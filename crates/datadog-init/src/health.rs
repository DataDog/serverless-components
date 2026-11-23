// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! The health endpoint, served when `DD_HEALTH_PORT` is set, as Go's health probe component.
//!
//! The answers simulate Go's health registry in serverless-init. They do not check the real
//! health of the components. The request parsing is minimal: it reads the method and path of the
//! request line, accepts CRLF or bare LF line endings, and closes the connection without an answer
//! for a request head over 8 KiB.

use std::time::{Duration, Instant};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Go's read and write timeout for the health endpoint.
const TIMEOUT: Duration = Duration::from_secs(1);

/// The largest request head read before answering.
const MAX_REQUEST_HEAD: usize = 8192;

/// The pause after a failed accept, so a persistent error such as running out of file
/// descriptors does not spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Go's health ping period. A component registers unhealthy and turns healthy at the first ping.
const FIRST_PING: Duration = Duration::from_secs(15);

/// The liveness components that Go's serverless-init registers, besides `dogstatsd-main`. Go lists
/// them in map order; here they are sorted.
const LIVENESS_COMPONENTS: [&str; 5] = [
    "aggregator",
    "tagger-store",
    "tagger-workloadmeta",
    "workloadmeta-puller",
    "workloadmeta-store",
];

/// The readiness-only components that Go's serverless-init registers.
const READINESS_COMPONENTS: [&str; 1] = ["forwarder"];

/// Go's health routes. Every path other than `/ready` and `/startup` is answered as `/live`.
#[derive(Clone, Copy, Debug)]
pub enum Route {
    Live,
    Ready,
    Startup,
}

impl Route {
    fn from_path(path: &str) -> Self {
        match path.split('?').next().unwrap_or_default() {
            "/ready" => Route::Ready,
            "/startup" => Route::Startup,
            _ => Route::Live,
        }
    }
}

/// One of Go's health catalogs: the checker's own `healthcheck` entry and the components, all
/// unhealthy before the first ping. An empty catalog has no entries at all.
fn catalog_status(
    components: &[&'static str],
    pinged: bool,
) -> (Vec<&'static str>, Vec<&'static str>) {
    if components.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let mut healthy = vec!["healthcheck"];
    let mut unhealthy = Vec::new();
    if pinged {
        healthy.extend_from_slice(components);
    } else {
        unhealthy.extend_from_slice(components);
    }
    (healthy, unhealthy)
}

/// Go's status code and body for the route, `elapsed` after the start. `dogstatsd` says whether
/// the DogStatsD server, and so its `dogstatsd-main` component, runs.
pub fn response(route: Route, elapsed: Duration, dogstatsd: bool) -> (u16, String) {
    let pinged = elapsed >= FIRST_PING;
    let mut liveness: Vec<&'static str> = LIVENESS_COMPONENTS.to_vec();
    if dogstatsd {
        liveness.insert(1, "dogstatsd-main");
    }
    let (healthy, unhealthy) = match route {
        Route::Live => catalog_status(&liveness, pinged),
        Route::Ready => {
            let (mut healthy, mut unhealthy) = catalog_status(&liveness, pinged);
            let (ready_healthy, ready_unhealthy) = catalog_status(&READINESS_COMPONENTS, pinged);
            healthy.extend(ready_healthy);
            unhealthy.extend(ready_unhealthy);
            (healthy, unhealthy)
        }
        Route::Startup => catalog_status(&[], pinged),
    };
    // Go marshals an empty list as null, because it is a nil slice.
    let list = |names: Vec<&'static str>| (!names.is_empty()).then_some(names);
    let status = if unhealthy.is_empty() { 200 } else { 500 };
    let body = json!({ "Healthy": list(healthy), "Unhealthy": list(unhealthy) }).to_string();
    (status, body)
}

/// Listens on every interface on the port, as Go does.
pub async fn bind(port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind(("0.0.0.0", port)).await
}

/// Answers every request on the listener, timing the simulated registry from `started`.
pub async fn serve(listener: TcpListener, started: Instant, dogstatsd: bool) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(answer(stream, started, dogstatsd));
            }
            Err(error) => {
                tracing::debug!("Health endpoint failed to accept a connection: {error}");
                tokio::time::sleep(ACCEPT_BACKOFF).await;
            }
        }
    }
}

async fn answer(mut stream: TcpStream, started: Instant, dogstatsd: bool) {
    let exchange = async {
        let mut head = Vec::new();
        let mut buffer = [0u8; 1024];
        while !head.windows(4).any(|window| window == b"\r\n\r\n")
            && !head.windows(2).any(|window| window == b"\n\n")
        {
            let read = stream.read(&mut buffer).await?;
            if read == 0 || head.len() + read > MAX_REQUEST_HEAD {
                return Ok(());
            }
            head.extend_from_slice(&buffer[..read]);
        }
        let request_line = String::from_utf8_lossy(&head);
        let mut words = request_line.split_whitespace();
        let method = words.next().unwrap_or_default();
        let route = Route::from_path(words.next().unwrap_or_default());
        let (status, body) = response(route, started.elapsed(), dogstatsd);
        let reason = if status == 200 {
            "OK"
        } else {
            "Internal Server Error"
        };
        let mut reply = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        if method != "HEAD" {
            reply.push_str(&body);
        }
        stream.write_all(reply.as_bytes()).await?;
        stream.shutdown().await
    };
    match tokio::time::timeout(TIMEOUT, exchange).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::debug!("Health endpoint request failed: {error}"),
        Err(_) => tracing::debug!("Health endpoint request timed out"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_table() {
        let cases: &[(Route, u64, bool, u16, &str)] = &[
            (
                Route::Live,
                2,
                true,
                500,
                r#"{"Healthy":["healthcheck"],"Unhealthy":["aggregator","dogstatsd-main","tagger-store","tagger-workloadmeta","workloadmeta-puller","workloadmeta-store"]}"#,
            ),
            (
                Route::Live,
                15,
                true,
                200,
                r#"{"Healthy":["healthcheck","aggregator","dogstatsd-main","tagger-store","tagger-workloadmeta","workloadmeta-puller","workloadmeta-store"],"Unhealthy":null}"#,
            ),
            (
                Route::Live,
                15,
                false,
                200,
                r#"{"Healthy":["healthcheck","aggregator","tagger-store","tagger-workloadmeta","workloadmeta-puller","workloadmeta-store"],"Unhealthy":null}"#,
            ),
            (
                Route::Ready,
                2,
                false,
                500,
                r#"{"Healthy":["healthcheck","healthcheck"],"Unhealthy":["aggregator","tagger-store","tagger-workloadmeta","workloadmeta-puller","workloadmeta-store","forwarder"]}"#,
            ),
            (
                Route::Ready,
                15,
                false,
                200,
                r#"{"Healthy":["healthcheck","aggregator","tagger-store","tagger-workloadmeta","workloadmeta-puller","workloadmeta-store","healthcheck","forwarder"],"Unhealthy":null}"#,
            ),
            (
                Route::Startup,
                2,
                true,
                200,
                r#"{"Healthy":null,"Unhealthy":null}"#,
            ),
        ];
        for (route, seconds, dogstatsd, status, body) in cases {
            assert_eq!(
                response(*route, Duration::from_secs(*seconds), *dogstatsd),
                (*status, body.to_string()),
                "route {route:?} at {seconds} seconds, dogstatsd {dogstatsd}"
            );
        }
    }

    #[tokio::test]
    async fn serves_the_route_over_http() {
        let listener = bind(0).await.expect("bind");
        let address = listener.local_addr().expect("address");
        tokio::spawn(serve(listener, Instant::now(), false));
        let cases = [
            (
                "GET /startup HTTP/1.1\nHost: localhost\n\n",
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 33\r\nConnection: close\r\n\r\n{\"Healthy\":null,\"Unhealthy\":null}",
            ),
            (
                "HEAD /ready HTTP/1.1\r\nHost: localhost\r\n\r\n",
                "HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 160\r\nConnection: close\r\n\r\n",
            ),
        ];
        for (request, expected) in cases {
            let mut stream = TcpStream::connect(address).await.expect("connect");
            stream.write_all(request.as_bytes()).await.expect("write");
            let mut response = String::new();
            stream.read_to_string(&mut response).await.expect("read");
            assert_eq!(response, expected, "request {request:?}");
        }
    }
}
