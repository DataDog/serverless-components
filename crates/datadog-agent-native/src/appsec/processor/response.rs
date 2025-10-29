//! Response format definitions for different invocation types.
//!
//! This module defines how HTTP responses are expected to be structured based
//! on the invocation source (e.g., API Gateway, direct HTTP, Lambda, etc.).
//!
//! # Response Formats
//!
//! Different event sources use different response structures:
//! - **API Gateway**: Structured with `statusCode`, `headers`, `body` fields
//! - **Raw HTTP**: Direct body content, parsed as-is
//! - **Unknown**: Unsupported or unrecognized format
//!
//! # General Agent Usage
//!
//! For the general (non-Lambda) agent, most responses use the `Raw` format,
//! where the entire payload is treated as-is. The Lambda-specific formats
//! are kept for potential future use but are currently stubbed out.

use crate::appsec::processor::InvocationPayload;
// Lambda-specific imports commented out for general agent
// use crate::lifecycle::invocation::triggers::{body::Body, lowercase_key};

/// The expected format of a response payload.
///
/// Different invocation sources (API Gateway, Lambda Function URLs, direct HTTP)
/// use different response structures. This enum helps the processor determine
/// how to parse and extract security-relevant data from responses.
///
/// # Variants
///
/// - **ApiGatewayResponse**: Structured response with `statusCode`, `headers`, `body`
/// - **Raw**: Entire payload forwarded as-is (typical for direct HTTP)
/// - **Unknown**: Unsupported or unrecognized response format
///
/// # Usage
///
/// ```rust,ignore
/// use datadog_agent_native::appsec::processor::response::ExpectedResponseFormat;
///
/// let format = ExpectedResponseFormat::Raw;
/// if !format.is_unknown() {
///     // Process response based on format
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedResponseFormat {
    /// API Gateway-style integration responses (REST and HTTP APIs).
    ///
    /// Expected structure:
    /// ```json
    /// {
    ///   "statusCode": 200,
    ///   "headers": {"content-type": "application/json"},
    ///   "body": "{\"result\": \"success\"}"
    /// }
    /// ```
    ApiGatewayResponse,

    /// Raw response payload, forwarded as-is.
    ///
    /// The processor will attempt to parse the payload as JSON if possible.
    /// This is the standard format for direct HTTP requests.
    Raw,

    /// Unknown or unsupported response format.
    ///
    /// Used when the invocation source is not recognized or doesn't provide
    /// HTTP response data (e.g., SQS, SNS, Kinesis events).
    Unknown,
}
impl ExpectedResponseFormat {
    /// Checks if this format is unknown/unsupported.
    ///
    /// # Returns
    ///
    /// * `true` - Format is `Unknown`, response cannot be analyzed
    /// * `false` - Format is known, response can be processed
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// if format.is_unknown() {
    ///     // Skip response analysis for this invocation type
    ///     return;
    /// }
    /// ```
    pub(crate) const fn is_unknown(self) -> bool {
        matches!(self, Self::Unknown)
    }

    /// Parses response payload based on the expected format.
    ///
    /// This method is stubbed out for the general agent since Lambda-specific
    /// response parsing is not needed. In a Lambda context, this would parse
    /// the response bytes into the appropriate structure.
    ///
    /// # Arguments
    ///
    /// * `payload` - Raw response bytes to parse
    ///
    /// # Returns
    ///
    /// * `Ok(Some(payload))` - Successfully parsed response
    /// * `Ok(None)` - No response data available
    /// * `Err(e)` - JSON parsing failed
    ///
    /// # Note
    ///
    /// Currently returns `Ok(None)` as the general agent doesn't use this
    /// method. Response data is provided directly via `HttpTransaction`.
    #[allow(unused_variables)]
    #[allow(dead_code)]
    pub(crate) fn parse(
        self,
        payload: &[u8],
    ) -> serde_json::Result<Option<Box<dyn InvocationPayload>>> {
        // In a general agent, this is not used since we don't process Lambda invocation responses
        // Decision: Return None rather than implement unused parsing logic
        Ok(None)
    }
}
impl Default for ExpectedResponseFormat {
    /// Returns `Unknown` as the default format.
    ///
    /// This ensures safety by defaulting to "cannot analyze" rather than
    /// assuming a format that might be incorrect.
    fn default() -> Self {
        Self::Unknown
    }
}

// Lambda-specific response structures commented out for general agent
/*
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
#[serde(rename_all = "camelCase")]
struct ApiGatewayResponse {
    status_code: i64,
    #[serde(deserialize_with = "lowercase_key", default)]
    headers: HashMap<String, String>,
    #[serde(deserialize_with = "lowercase_key", default)]
    multi_value_headers: HashMap<String, Vec<String>>,
    #[serde(flatten)]
    body: Body,
}
impl InvocationPayload for ApiGatewayResponse {
    #[cfg_attr(coverage_nightly, coverage(off))] // Only here to satisfy contract
    fn corresponding_response_format(&self) -> ExpectedResponseFormat {
        ExpectedResponseFormat::ApiGatewayResponse
    }

    fn response_status_code(&self) -> Option<i64> {
        Some(self.status_code)
    }
    fn response_headers_no_cookies(&self) -> HashMap<String, Vec<String>> {
        if self.multi_value_headers.is_empty() {
            self.headers
                .iter()
                .filter(|(k, _)| *k != "set-cookie")
                .map(|(k, v)| (k.clone(), vec![v.clone()]))
                .collect()
        } else {
            self.multi_value_headers.clone()
        }
    }
    fn response_body<'a>(&'a self) -> Option<Box<dyn std::io::Read + 'a>> {
        self.body.reader().ok().flatten()
    }
}

struct RawPayload {
    data: Vec<u8>,
}
impl InvocationPayload for RawPayload {
    #[cfg_attr(coverage_nightly, coverage(off))] // Only here to satisfy contract
    fn corresponding_response_format(&self) -> ExpectedResponseFormat {
        ExpectedResponseFormat::Raw
    }

    fn response_status_code(&self) -> Option<i64> {
        None
    }
    fn response_headers_no_cookies(&self) -> HashMap<String, Vec<String>> {
        HashMap::default()
    }
    fn response_body<'a>(&'a self) -> Option<Box<dyn std::io::Read + 'a>> {
        Some(Box::new(Cursor::new(&self.data)))
    }
}
*/
