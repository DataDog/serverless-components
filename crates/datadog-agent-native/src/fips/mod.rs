//! FIPS 140-2 compliance configuration for cryptographic operations.
//!
//! This module contains conditional compilation logic and functions for managing
//! FIPS (Federal Information Processing Standards) mode in the Datadog agent.
//! FIPS 140-2 is a US government security standard for cryptographic modules.
//!
//! # FIPS Mode
//!
//! When compiled with the `fips` feature, the agent uses AWS-LC (AWS Libcrypto)
//! which is a FIPS 140-2 validated cryptographic library. This ensures all
//! cryptographic operations (TLS, hashing, etc.) meet federal security requirements.
//!
//! # Feature Flags
//!
//! - **fips**: Enables FIPS mode with AWS-LC
//! - **default**: Standard mode (incompatible with FIPS mode)
//!
//! These features are mutually exclusive. Attempting to enable both will result
//! in a compile error.
//!
//! # AWS GovCloud
//!
//! In AWS GovCloud regions (`us-gov-*`), FIPS mode is enabled by default to comply
//! with federal requirements. This can be overridden with `DD_LAMBDA_FIPS_MODE=false`.
//!
//! # Configuration
//!
//! FIPS mode is controlled by:
//! - **Compile-time**: `fips` feature flag (must be set at build time)
//! - **Runtime**: `DD_LAMBDA_FIPS_MODE` environment variable
//! - **Region**: Automatic detection of AWS GovCloud regions
//!
//! # Differences Between FIPS and Standard Modes
//!
//! When FIPS mode is enabled:
//! 1. **Crypto Provider**: Uses AWS-LC instead of ring/rustls-native-certs
//! 2. **TLS Configuration**: All TLS operations use FIPS-validated algorithms
//! 3. **AWS API Endpoints**: Uses `-fips` suffix (e.g., `sqs-fips.us-gov-west-1.amazonaws.com`)
//!
//! # Example
//!
//! ```rust,ignore
//! use datadog_agent_native::fips;
//!
//! // Setup FIPS crypto provider (if enabled)
//! fips::prepare_client_provider()?;
//!
//! // Log FIPS status for debugging
//! fips::log_fips_status("us-gov-west-1");
//!
//! // Compute AWS API endpoint with FIPS suffix if needed
//! let host = fips::compute_aws_api_host(&"sqs".to_string(), &"us-gov-west-1".to_string(), "amazonaws.com");
//! // In FIPS mode: "sqs-fips.us-gov-west-1.amazonaws.com"
//! // In standard mode: "sqs.us-gov-west-1.amazonaws.com"
//! ```

use std::env;
#[cfg(feature = "fips")]
use std::io::Error;
use std::io::Result;
use tracing::debug;

// Compile-time check to prevent invalid feature combinations
// Decision: Fail at compile time rather than runtime for configuration errors
#[cfg(all(feature = "default", feature = "fips"))]
compile_error!("When building in fips mode, the default feature must be disabled");

/// Determines if the Lambda runtime layer would enable FIPS mode.
///
/// This function checks whether the Datadog Lambda runtime layer would enable
/// FIPS mode based on the AWS region and environment variables. This is used
/// to detect mismatches between extension and runtime FIPS configurations.
///
/// # Arguments
///
/// * `region` - AWS region (e.g., "us-gov-west-1", "us-east-1")
///
/// # Returns
///
/// `true` if FIPS mode would be enabled in the runtime layer, `false` otherwise.
///
/// # Detection Logic
///
/// 1. **Explicit**: If `DD_LAMBDA_FIPS_MODE=true`, always return `true`
/// 2. **Explicit**: If `DD_LAMBDA_FIPS_MODE=false`, always return `false`
/// 3. **Auto-detect**: If not set, return `true` for AWS GovCloud regions (`us-gov-*`)
/// 4. **Default**: For other regions, return `false`
///
/// # AWS GovCloud
///
/// GovCloud regions require FIPS by default to comply with federal security
/// requirements. This function returns `true` for `us-gov-*` regions unless
/// explicitly overridden.
///
/// # Example
///
/// ```rust,ignore
/// // GovCloud region - defaults to FIPS
/// assert!(runtime_layer_would_enable_fips_mode("us-gov-west-1"));
///
/// // Commercial region - defaults to non-FIPS
/// assert!(!runtime_layer_would_enable_fips_mode("us-east-1"));
///
/// // Explicit override
/// std::env::set_var("DD_LAMBDA_FIPS_MODE", "true");
/// assert!(runtime_layer_would_enable_fips_mode("us-east-1"));
/// ```
#[must_use]
pub fn runtime_layer_would_enable_fips_mode(region: &str) -> bool {
    // Decision: Check if region is AWS GovCloud (requires FIPS by default)
    let is_gov_region = region.starts_with("us-gov-");

    // Decision: Allow explicit override via DD_LAMBDA_FIPS_MODE, otherwise default to region-based logic
    // Note: We default to `is_gov_region` rather than a hardcoded value
    // This means GovCloud lambdas run in FIPS mode by default unless explicitly disabled
    env::var("DD_LAMBDA_FIPS_MODE")
        .map(|val| val.to_lowercase() == "true")
        .unwrap_or(is_gov_region)
}

/// Checks for FIPS mode mismatch between extension and runtime layers.
///
/// This function logs a warning if the FIPS configuration of the extension layer
/// doesn't match what the runtime layer would use. Mismatches can cause inconsistent
/// behavior and should be resolved.
///
/// # Arguments
///
/// * `region` - AWS region for checking expected runtime layer behavior
///
/// # Mismatch Scenarios
///
/// - **Extension in standard mode, runtime in FIPS mode**: Deploy FIPS extension
/// - **Extension in FIPS mode, runtime in standard mode**: Deploy standard extension or set `DD_LAMBDA_FIPS_MODE=true`
///
/// # Example
///
/// ```rust,ignore
/// // Check for configuration mismatches
/// check_fips_mode_mismatch("us-gov-west-1");
/// // If extension is non-FIPS and region is GovCloud, logs warning
/// ```
pub fn check_fips_mode_mismatch(region: &str) {
    // Decision: Check if runtime would enable FIPS based on region/env
    if runtime_layer_would_enable_fips_mode(region) {
        // Decision: Only log warning if THIS extension is non-FIPS (conditional compilation)
        // Runtime would enable FIPS, but extension doesn't have FIPS feature
        #[cfg(not(feature = "fips"))]
        debug!(
            "FIPS mode is disabled in this Extension layer but would be enabled in the runtime layer based on region and environment settings. Deploy the FIPS version of the Extension layer or set DD_LAMBDA_FIPS_MODE=false to ensure consistent FIPS behavior."
        );
    } else {
        // Decision: Only log warning if THIS extension IS fips (conditional compilation)
        // Runtime would NOT enable FIPS, but extension has FIPS feature
        #[cfg(feature = "fips")]
        debug!(
            "FIPS mode is enabled in this Extension layer but would be disabled in the runtime layer based on region and environment settings. Set DD_LAMBDA_FIPS_MODE=true or deploy the standard (non-FIPS) version of the Extension layer to ensure consistent FIPS behavior."
        );
    }
}

/// Logs the current FIPS mode status (FIPS-enabled build).
///
/// This function logs that FIPS mode is enabled and checks for any configuration
/// mismatches with the runtime layer.
///
/// # Platform
///
/// This version is only compiled when the `fips` feature is enabled.
///
/// # Arguments
///
/// * `region` - AWS region for mismatch detection
///
/// # Example
///
/// ```rust,ignore
/// fips::log_fips_status("us-gov-west-1");
/// // Output: "FIPS mode is enabled"
/// ```
#[cfg(feature = "fips")]
pub fn log_fips_status(region: &str) {
    debug!("FIPS mode is enabled");
    check_fips_mode_mismatch(region);
}

/// Logs the current FIPS mode status (standard build).
///
/// This function logs that FIPS mode is disabled and checks for any configuration
/// mismatches with the runtime layer.
///
/// # Platform
///
/// This version is only compiled when the `fips` feature is NOT enabled.
///
/// # Arguments
///
/// * `region` - AWS region for mismatch detection
///
/// # Example
///
/// ```rust,ignore
/// fips::log_fips_status("us-east-1");
/// // Output: "FIPS mode is disabled"
/// ```
#[cfg(not(feature = "fips"))]
pub fn log_fips_status(region: &str) {
    debug!("FIPS mode is disabled");
    check_fips_mode_mismatch(region);
}

/// Sets up the cryptographic provider for TLS operations (FIPS mode).
///
/// In FIPS mode, this function installs the AWS-LC crypto provider as the default
/// for rustls TLS operations. AWS-LC is a FIPS 140-2 validated cryptographic library.
///
/// # Platform
///
/// This version is only compiled when the `fips` feature is enabled.
///
/// # Returns
///
/// * `Ok(())` - Crypto provider successfully installed
/// * `Err` - Failed to install the FIPS provider
///
/// # Errors
///
/// Returns an error if the FIPS crypto provider cannot be installed. This is a
/// fatal error that should prevent the agent from starting.
///
/// # Example
///
/// ```rust,ignore
/// fips::prepare_client_provider()?;
/// // AWS-LC FIPS crypto provider is now the default for all TLS operations
/// ```
#[cfg(feature = "fips")]
pub fn prepare_client_provider() -> Result<()> {
    // Decision: Install AWS-LC FIPS provider as the default for rustls
    // This ensures all TLS operations use FIPS-validated cryptography
    rustls::crypto::default_fips_provider()
        .install_default()
        .map_err(|e| {
            // Decision: Convert rustls error to io::Error for consistent error handling
            Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Failed to set up fips provider: {e:?}"),
            )
        })
}

/// Sets up the cryptographic provider for TLS operations (standard mode).
///
/// In standard (non-FIPS) mode, this is a no-op. The default rustls crypto
/// provider (ring or rustls-native-certs) is used automatically.
///
/// # Platform
///
/// This version is only compiled when the `fips` feature is NOT enabled.
///
/// # Returns
///
/// Always returns `Ok(())`.
///
/// # Example
///
/// ```rust,ignore
/// fips::prepare_client_provider()?;
/// // No-op in standard mode, default crypto provider is used
/// ```
#[cfg(not(feature = "fips"))]
// Decision: Allow unnecessary_wraps lint because the FIPS version can return an error
// This keeps the API consistent across both feature configurations
#[allow(clippy::unnecessary_wraps)]
pub fn prepare_client_provider() -> Result<()> {
    // Decision: No-op in non-FIPS mode (default provider is already configured)
    Ok(())
}

/// Computes AWS API endpoint hostname (standard mode).
///
/// In standard (non-FIPS) mode, returns a standard AWS API endpoint without
/// the `-fips` suffix.
///
/// # Platform
///
/// This version is only compiled when the `fips` feature is NOT enabled.
///
/// # Arguments
///
/// * `service` - AWS service name (e.g., "sqs", "s3", "lambda")
/// * `region` - AWS region (e.g., "us-east-1", "us-gov-west-1")
/// * `domain` - AWS domain (typically "amazonaws.com")
///
/// # Returns
///
/// Standard AWS API endpoint: `{service}.{region}.{domain}`
///
/// # Example
///
/// ```rust,ignore
/// let host = compute_aws_api_host(&"sqs".to_string(), &"us-east-1".to_string(), "amazonaws.com");
/// assert_eq!(host, "sqs.us-east-1.amazonaws.com");
/// ```
#[cfg(not(feature = "fips"))]
#[must_use]
pub fn compute_aws_api_host(service: &String, region: &String, domain: &str) -> String {
    // Decision: Use standard endpoint format (no -fips suffix in non-FIPS mode)
    format!("{service}.{region}.{domain}")
}

/// Computes AWS API endpoint hostname (FIPS mode).
///
/// In FIPS mode, returns an AWS API endpoint with the `-fips` suffix to ensure
/// all API calls go through FIPS-validated endpoints.
///
/// # Platform
///
/// This version is only compiled when the `fips` feature is enabled.
///
/// # Arguments
///
/// * `service` - AWS service name (e.g., "sqs", "s3", "lambda")
/// * `region` - AWS region (e.g., "us-east-1", "us-gov-west-1")
/// * `domain` - AWS domain (typically "amazonaws.com")
///
/// # Returns
///
/// FIPS AWS API endpoint: `{service}-fips.{region}.{domain}`
///
/// # Example
///
/// ```rust,ignore
/// let host = compute_aws_api_host(&"sqs".to_string(), &"us-gov-west-1".to_string(), "amazonaws.com");
/// assert_eq!(host, "sqs-fips.us-gov-west-1.amazonaws.com");
/// ```
///
/// # AWS FIPS Endpoints
///
/// Many AWS services provide separate FIPS endpoints that use FIPS-validated
/// TLS implementations. These endpoints are required for compliance in GovCloud
/// and other federal environments.
#[cfg(feature = "fips")]
#[must_use]
pub fn compute_aws_api_host(service: &String, region: &String, domain: &str) -> String {
    // Decision: Add -fips suffix to service name to use FIPS-validated AWS endpoints
    format!("{service}-fips.{region}.{domain}")
}
