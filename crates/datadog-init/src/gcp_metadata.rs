// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Reads the instance ID, the region and the project from the Google Cloud metadata server, as Go
//! serverless-init does on Google Cloud Run (`GetMetaData` in `cloudservice/cloudrun.go`). The
//! trace agent's metadata fetch in `env_verifier.rs` is not reused: it makes one recursive request
//! with a 100-millisecond timeout, while Go makes three requests of 300 milliseconds each.

use std::time::Duration;

use datadog_fips::reqwest_adapter::create_reqwest_client_builder;
use tracing::{info, warn};

use crate::platform::GcpMetadata;

pub const BASE_URL: &str = "http://metadata.google.internal/computeMetadata/v1";

/// Go's timeout for each request. The three requests run in parallel, so Go's total timeout of
/// six times this is never reached.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(300);

/// Requests the three values from `base_url` in parallel. A failed request gives an empty value.
pub async fn fetch(base_url: &str) -> GcpMetadata {
    let client = match create_reqwest_client_builder()
        .and_then(|builder| Ok(builder.timeout(REQUEST_TIMEOUT).build()?))
    {
        Ok(client) => client,
        Err(error) => {
            warn!("Failed to build the Google Cloud metadata client: {error}");
            return GcpMetadata::default();
        }
    };
    let get = |path: &'static str| {
        let client = client.clone();
        let url = format!("{base_url}{path}");
        async move { get(&client, &url).await.unwrap_or_default() }
    };
    let (instance_id, region, project_id) = tokio::join!(
        get("/instance/id"),
        get("/instance/region"),
        get("/project/project-id"),
    );
    GcpMetadata {
        instance_id,
        region: region.rsplit('/').next().unwrap_or_default().to_string(),
        project_id,
    }
}

/// The lowercased body of a response with status 200, the only status Go accepts.
async fn get(client: &reqwest::Client, url: &str) -> Option<String> {
    let response = client
        .get(url)
        .header("Metadata-Flavor", "Google")
        .send()
        .await
        .map_err(|_| info!("unable to get the requested metadata"))
        .ok()?;
    if response.status() != reqwest::StatusCode::OK {
        info!(
            "metadata request failed with HTTP status {}",
            response.status().as_u16()
        );
        return None;
    }
    let body = response
        .text()
        .await
        .map_err(|_| warn!("unable to read metadata body"))
        .ok()?;
    Some(body.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A server that answers each path with its status line and body, and anything else with 404.
    async fn start_server(
        answers: &'static [(&'static str, &'static str, &'static str)],
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buffer = [0u8; 4096];
                    let read = stream.read(&mut buffer).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..read]);
                    let path = request.split_whitespace().nth(1).unwrap_or_default();
                    let has_header = request.contains("metadata-flavor: Google");
                    let response = match answers.iter().find(|(known, _, _)| *known == path) {
                        Some((_, status, body)) if has_header => format!(
                            "HTTP/1.1 {status}\r\ncontent-length: {}\r\n\r\n{body}",
                            body.len()
                        ),
                        _ => "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n".to_string(),
                    };
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn fetch_table() {
        let all = start_server(&[
            ("/instance/id", "200 OK", "Instance-1"),
            (
                "/instance/region",
                "200 OK",
                "projects/123/regions/us-central1",
            ),
            ("/project/project-id", "200 OK", "Project-1"),
        ])
        .await;
        // Only status 200 counts.
        let only_project = start_server(&[
            (
                "/instance/id",
                "203 Non-Authoritative Information",
                "instance-1",
            ),
            ("/project/project-id", "200 OK", "project-1"),
        ])
        .await;
        let cases = [
            (
                all,
                GcpMetadata {
                    instance_id: "instance-1".to_string(),
                    region: "us-central1".to_string(),
                    project_id: "project-1".to_string(),
                },
            ),
            (
                only_project,
                GcpMetadata {
                    project_id: "project-1".to_string(),
                    ..GcpMetadata::default()
                },
            ),
            ("http://127.0.0.1:1".to_string(), GcpMetadata::default()),
        ];
        for (base_url, expected) in cases {
            assert_eq!(fetch(&base_url).await, expected, "{base_url}");
        }
    }
}
