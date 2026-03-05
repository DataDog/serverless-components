// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use core::time::Duration;
use datadog_fips::reqwest_adapter::create_reqwest_client_builder;
use hyper::{
    header,
    http::{self, HeaderMap},
    Response, StatusCode,
};
use libdd_common::hyper_migration;
use serde_json::json;
use std::error::Error;
use tracing::{debug, error};

/// Does two things:
/// 1. Logs the given message. A success status code (within 200-299) will cause an info log to be
///    written, otherwise error will be written.
/// 2. Returns the given message in the body of JSON response with the given status code.
///
/// Response body format:
/// {
///     "message": message
/// }
pub fn log_and_create_http_response(
    message: &str,
    status: StatusCode,
) -> http::Result<Response<hyper_migration::Body>> {
    if status.is_success() {
        debug!("{message}");
    } else {
        error!("{message}");
    }
    let body = json!({ "message": message }).to_string();
    Response::builder()
        .status(status)
        .body(hyper_migration::Body::from(body))
}

/// Does two things:
/// 1. Logs the given message
/// 2. Returns the rate_by_service map to use to set the sampling priority in the body of JSON
///    response with the given status code.
///
/// Response body format:
/// {
///     "rate_by_service": {
///         "service:,env:":1
///     }
/// }
pub fn log_and_create_traces_success_http_response(
    message: &str,
    status: StatusCode,
) -> http::Result<hyper_migration::HttpResponse> {
    debug!("{message}");
    let body = json!({"rate_by_service":{"service:,env:":1}}).to_string();
    Response::builder()
        .status(status)
        .body(hyper_migration::Body::from(body))
}

/// Takes a request's header map, and verifies that the "content-length" and/or "Transfer-Encoding" header
/// is present, valid, and less than the given max_content_length.
///
/// Will return None if no issues are found. Otherwise logs an error (with the given prefix) and
/// returns and HTTP Response with the appropriate error status code.
pub fn verify_request_content_length(
    header_map: &HeaderMap,
    max_content_length: usize,
    error_message_prefix: &str,
) -> Option<http::Result<hyper_migration::HttpResponse>> {
    let content_length_header = match header_map.get(header::CONTENT_LENGTH) {
        Some(res) => res,
        None => {
            if let Some(transfer_encoding_header) = header_map.get(header::TRANSFER_ENCODING) {
                debug!(
                    "Transfer-Encoding header is present: {:?}",
                    transfer_encoding_header
                );
                return None;
            }
            return Some(log_and_create_http_response(
                &format!(
                    "{error_message_prefix}: Missing Content-Length and Transfer-Encoding header"
                ),
                StatusCode::LENGTH_REQUIRED,
            ));
        }
    };
    let header_as_string = match content_length_header.to_str() {
        Ok(res) => res,
        Err(_) => {
            return Some(log_and_create_http_response(
                &format!("{error_message_prefix}: Invalid Content-Length header"),
                StatusCode::BAD_REQUEST,
            ));
        }
    };
    let content_length = match header_as_string.to_string().parse::<usize>() {
        Ok(res) => res,
        Err(_) => {
            return Some(log_and_create_http_response(
                &format!("{error_message_prefix}: Invalid Content-Length header"),
                StatusCode::BAD_REQUEST,
            ));
        }
    };
    if content_length > max_content_length {
        return Some(log_and_create_http_response(
            &format!("{error_message_prefix}: Payload too large"),
            StatusCode::PAYLOAD_TOO_LARGE,
        ));
    }
    None
}

/// Environment variable set by the Lambda runtime to indicate the initialisation type.
/// Lambda Lite (web function / snap-start mode) sets this to `"native-http"`;
/// standard on-demand invocations set it to `"on-demand"`.
const ENV_LAMBDA_INIT_TYPE: &str = "AWS_LAMBDA_INITIALIZATION_TYPE";

/// Returns true if the current environment is Lambda Lite (web function / snap start mode).
///
/// Determined by checking [`ENV_LAMBDA_INIT_TYPE`]` == "native-http"`. This is used to gate
/// behaviour specific to long-running web server deployments on Lambda Lite.
pub fn is_lambda_lite() -> bool {
    is_lambda_lite_from_env(std::env::var(ENV_LAMBDA_INIT_TYPE).ok().as_deref())
}

fn is_lambda_lite_from_env(val: Option<&str>) -> bool {
    val == Some("native-http")
}

/// Builds a reqwest client with optional proxy configuration and timeout.
/// Uses rustls TLS by default. FIPS-compliant TLS is available via the fips feature
pub fn build_client(
    proxy_url: Option<&str>,
    timeout: Duration,
) -> Result<reqwest::Client, Box<dyn Error>> {
    let mut builder = create_reqwest_client_builder()?.timeout(timeout);
    if let Some(proxy) = proxy_url {
        builder = builder.proxy(reqwest::Proxy::https(proxy)?);
    }
    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    use http_body_util::BodyExt;
    use hyper::header;
    use hyper::HeaderMap;
    use hyper::StatusCode;
    use libdd_common::hyper_migration;

    use super::is_lambda_lite_from_env;
    use super::verify_request_content_length;

    #[test]
    fn test_is_lambda_lite_native_http() {
        assert!(is_lambda_lite_from_env(Some("native-http")));
    }

    #[test]
    fn test_is_lambda_lite_on_demand() {
        assert!(!is_lambda_lite_from_env(Some("on-demand")));
    }

    #[test]
    fn test_is_lambda_lite_empty_string() {
        assert!(!is_lambda_lite_from_env(Some("")));
    }

    #[test]
    fn test_is_lambda_lite_unset() {
        assert!(!is_lambda_lite_from_env(None));
    }

    fn create_test_headers_with_content_length(val: &str) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert(header::CONTENT_LENGTH, val.parse().unwrap());
        map
    }

    async fn get_response_body_as_string(response: hyper_migration::HttpResponse) -> String {
        let body = response.into_body();
        let bytes = body.collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.into_iter().collect()).unwrap()
    }

    #[tokio::test]
    async fn test_request_content_length_missing() {
        let verify_result = verify_request_content_length(&HeaderMap::new(), 1, "Test Prefix");
        assert!(verify_result.is_some());

        let response = verify_result.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::LENGTH_REQUIRED);
        assert_eq!(
            get_response_body_as_string(response).await,
            "{\"message\":\"Test Prefix: Missing Content-Length and Transfer-Encoding header\"}"
                .to_string()
        );
    }

    #[tokio::test]
    async fn test_request_content_length_cant_convert_to_str() {
        let verify_result = verify_request_content_length(
            &create_test_headers_with_content_length("❤❤❤❤❤❤❤"),
            1,
            "Test Prefix",
        );
        assert!(verify_result.is_some());

        let response = verify_result.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            get_response_body_as_string(response).await,
            "{\"message\":\"Test Prefix: Invalid Content-Length header\"}".to_string()
        );
    }

    #[tokio::test]
    async fn test_request_content_length_cant_convert_to_usize() {
        let verify_result = verify_request_content_length(
            &create_test_headers_with_content_length("not_an_int"),
            1,
            "Test Prefix",
        );
        assert!(verify_result.is_some());

        let response = verify_result.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            get_response_body_as_string(response).await,
            "{\"message\":\"Test Prefix: Invalid Content-Length header\"}".to_string()
        );
    }

    #[tokio::test]
    async fn test_request_content_length_too_long() {
        let verify_result = verify_request_content_length(
            &create_test_headers_with_content_length("100"),
            1,
            "Test Prefix",
        );

        assert!(verify_result.is_some());

        let response = verify_result.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            get_response_body_as_string(response).await,
            "{\"message\":\"Test Prefix: Payload too large\"}".to_string()
        );
    }
}
