// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Simple mock HTTP server for testing flushers

use http_body_util::BodyExt;
use hyper::{body::Incoming, Request, Response};
use hyper_util::rt::TokioIo;
use libdd_common::hyper_migration;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

#[derive(Clone, Debug)]
pub struct ReceivedRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Clone)]
pub struct MockServer {
    pub addr: SocketAddr,
    pub received_requests: Arc<Mutex<Vec<ReceivedRequest>>>,
}

impl MockServer {
    /// Start a mock HTTP server on a random port
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Failed to bind mock server");
        let addr = listener.local_addr().expect("Failed to get local addr");

        let received_requests = Arc::new(Mutex::new(Vec::new()));
        let requests_clone = received_requests.clone();

        tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(_) => break,
                };

                let io = TokioIo::new(stream);
                let requests = requests_clone.clone();

                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |req: Request<Incoming>| {
                        let requests = requests.clone();
                        async move {
                            // Capture the request
                            let method = req.method().to_string();
                            let path = req.uri().path().to_string();
                            let headers: Vec<(String, String)> = req
                                .headers()
                                .iter()
                                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                                .collect();

                            // Read the body
                            let body_bytes = req
                                .into_body()
                                .collect()
                                .await
                                .map(|collected| collected.to_bytes().to_vec())
                                .unwrap_or_default();

                            // Store the request
                            requests.lock().unwrap().push(ReceivedRequest {
                                method,
                                path,
                                headers,
                                body: body_bytes,
                            });

                            // Return 200 OK
                            Ok::<_, hyper::http::Error>(
                                Response::builder()
                                    .status(200)
                                    .body(hyper_migration::Body::from(r#"{"ok":true}"#))
                                    .unwrap(),
                            )
                        }
                    });

                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service)
                        .await;
                });
            }
        });

        MockServer {
            addr,
            received_requests,
        }
    }

    /// Get the base URL of the mock server
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Get all received requests
    #[allow(dead_code)]
    pub fn get_requests(&self) -> Vec<ReceivedRequest> {
        self.received_requests.lock().unwrap().clone()
    }

    /// Get requests matching a path
    pub fn get_requests_for_path(&self, path: &str) -> Vec<ReceivedRequest> {
        self.received_requests
            .lock()
            .unwrap()
            .iter()
            .filter(|req| req.path == path)
            .cloned()
            .collect()
    }

    /// Clear all received requests
    #[allow(dead_code)]
    pub fn clear_requests(&self) {
        self.received_requests.lock().unwrap().clear();
    }
}
