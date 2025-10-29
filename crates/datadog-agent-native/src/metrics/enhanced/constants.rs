//! Constants for enhanced metrics including metric names and pricing calculations.
//!
//! This module defines:
//! - **Pricing constants**: Lambda cost estimation formulas
//! - **Metric names**: Standard Datadog metric identifiers
//! - **Configuration**: Monitoring intervals and environment variables
//!
//! # Lambda Pricing Model (as of 2024)
//!
//! Lambda costs = Base invocation price + (GB-seconds × architecture-specific rate)
//!
//! Example calculation:
//! - Base: $0.0000002 per request
//! - Compute: $0.0000166667 per GB-second (x86) or $0.0000133334 per GB-second (ARM)
//!
//! For a 1024MB function running 500ms on x86:
//! - Base: $0.0000002
//! - Compute: 1 GB × 0.5s × $0.0000166667 = $0.00000833335
//! - Total: $0.00000853335 per invocation

/// Path to the `/tmp` directory in Lambda environments.
///
/// Lambda provides a writable `/tmp` directory with 512MB-10GB of storage
/// (configurable). This path is monitored for disk usage metrics.
pub const TMP_PATH: &str = "/tmp/";

// Enhanced Metrics Names
//
// These constants define the standard Datadog metric names for Lambda enhanced metrics.
// All metrics use the `aws.lambda.enhanced.*` prefix for consistency with Datadog's
// Lambda monitoring product.

// Memory Metrics

/// Maximum memory used during invocation (in bytes).
///
/// Metric type: Gauge
/// Unit: bytes
pub const MAX_MEMORY_USED_METRIC: &str = "aws.lambda.enhanced.max_memory_used";

/// Configured memory size for the Lambda function (in MB).
///
/// Metric type: Gauge
/// Unit: megabytes
pub const MEMORY_SIZE_METRIC: &str = "aws.lambda.enhanced.memorysize";

// Duration Metrics

/// Actual runtime duration (wall-clock time from start to finish).
///
/// Metric type: Distribution
/// Unit: milliseconds
pub const RUNTIME_DURATION_METRIC: &str = "aws.lambda.enhanced.runtime_duration";

/// Billed duration (rounded up to nearest 100ms for AWS billing).
///
/// Metric type: Distribution
/// Unit: milliseconds
pub const BILLED_DURATION_METRIC: &str = "aws.lambda.enhanced.billed_duration";

/// Total duration including cold start initialization.
///
/// Metric type: Distribution
/// Unit: milliseconds
pub const DURATION_METRIC: &str = "aws.lambda.enhanced.duration";

/// Post-runtime duration (cleanup after handler returns).
///
/// Metric type: Distribution
/// Unit: milliseconds
pub const POST_RUNTIME_DURATION_METRIC: &str = "aws.lambda.enhanced.post_runtime_duration";

/// Cold start initialization duration.
///
/// Metric type: Distribution
/// Unit: milliseconds
pub const INIT_DURATION_METRIC: &str = "aws.lambda.enhanced.init_duration";

/// Response latency (time to first byte for streaming responses).
///
/// Metric type: Distribution
/// Unit: milliseconds
pub const RESPONSE_LATENCY_METRIC: &str = "aws.lambda.enhanced.response_latency";

/// Total response duration (complete response generation time).
///
/// Metric type: Distribution
/// Unit: milliseconds
pub const RESPONSE_DURATION_METRIC: &str = "aws.lambda.enhanced.response_duration";

// Cost Metrics

/// Estimated cost for this invocation (in USD).
///
/// Calculated using: BASE_LAMBDA_INVOCATION_PRICE + (GB-seconds × architecture rate)
///
/// Metric type: Gauge
/// Unit: USD
pub const ESTIMATED_COST_METRIC: &str = "aws.lambda.enhanced.estimated_cost";

// I/O Metrics

/// Response payload size (in bytes).
///
/// Metric type: Distribution
/// Unit: bytes
pub const PRODUCED_BYTES_METRIC: &str = "aws.lambda.enhanced.produced_bytes";

/// Network bytes received (RX) during invocation.
///
/// Metric type: Count
/// Unit: bytes
pub const RX_BYTES_METRIC: &str = "aws.lambda.enhanced.rx_bytes";

/// Network bytes transmitted (TX) during invocation.
///
/// Metric type: Count
/// Unit: bytes
pub const TX_BYTES_METRIC: &str = "aws.lambda.enhanced.tx_bytes";

/// Total network throughput (RX + TX).
///
/// Metric type: Count
/// Unit: bytes
pub const TOTAL_NETWORK_METRIC: &str = "aws.lambda.enhanced.total_network";

// Error Metrics

/// Count of out-of-memory errors.
///
/// Incremented when Lambda terminates due to memory exhaustion.
///
/// Metric type: Count
pub const OUT_OF_MEMORY_METRIC: &str = "aws.lambda.enhanced.out_of_memory";

/// Count of timeout errors.
///
/// Incremented when Lambda terminates due to timeout.
///
/// Metric type: Count
pub const TIMEOUTS_METRIC: &str = "aws.lambda.enhanced.timeouts";

/// Count of general errors (uncaught exceptions).
///
/// Metric type: Count
pub const ERRORS_METRIC: &str = "aws.lambda.enhanced.errors";

// Invocation Metrics

/// Count of Lambda invocations.
///
/// Metric type: Count
pub const INVOCATIONS_METRIC: &str = "aws.lambda.enhanced.invocations";

/// Count of Lambda shutdowns (instance terminations).
///
/// Metric type: Count
pub const SHUTDOWNS_METRIC: &str = "aws.lambda.enhanced.shutdowns";

/// Unused cold start indicator.
///
/// Tracks cold starts that don't process requests (sandbox pre-warming).
///
/// Metric type: Count
pub const UNUSED_INIT: &str = "aws.lambda.enhanced.unused_init";

// CPU Metrics

/// CPU time spent in system (kernel) mode.
///
/// Metric type: Distribution
/// Unit: milliseconds
pub const CPU_SYSTEM_TIME_METRIC: &str = "aws.lambda.enhanced.cpu_system_time";

/// CPU time spent in user mode.
///
/// Metric type: Distribution
/// Unit: milliseconds
pub const CPU_USER_TIME_METRIC: &str = "aws.lambda.enhanced.cpu_user_time";

/// Total CPU time (system + user).
///
/// Metric type: Distribution
/// Unit: milliseconds
pub const CPU_TOTAL_TIME_METRIC: &str = "aws.lambda.enhanced.cpu_total_time";

/// CPU utilization as a percentage (0-100%).
///
/// Calculated as: (CPU time / wall time) × 100
///
/// Metric type: Gauge
/// Unit: percent
pub const CPU_TOTAL_UTILIZATION_PCT_METRIC: &str = "aws.lambda.enhanced.cpu_total_utilization_pct";

/// CPU utilization as a ratio (0.0-1.0).
///
/// Metric type: Gauge
/// Unit: ratio
pub const CPU_TOTAL_UTILIZATION_METRIC: &str = "aws.lambda.enhanced.cpu_total_utilization";

/// Number of CPU cores available.
///
/// Metric type: Gauge
/// Unit: count
pub const NUM_CORES_METRIC: &str = "aws.lambda.enhanced.num_cores";

/// Maximum CPU utilization observed.
///
/// Metric type: Gauge
/// Unit: percent
pub const CPU_MAX_UTILIZATION_METRIC: &str = "aws.lambda.enhanced.cpu_max_utilization";

/// Minimum CPU utilization observed.
///
/// Metric type: Gauge
/// Unit: percent
pub const CPU_MIN_UTILIZATION_METRIC: &str = "aws.lambda.enhanced.cpu_min_utilization";

// Filesystem Metrics (/tmp directory)

/// Maximum available space in `/tmp` (total capacity).
///
/// Metric type: Gauge
/// Unit: bytes
pub const TMP_MAX_METRIC: &str = "aws.lambda.enhanced.tmp_max";

/// Current space used in `/tmp`.
///
/// Metric type: Gauge
/// Unit: bytes
pub const TMP_USED_METRIC: &str = "aws.lambda.enhanced.tmp_used";

/// Available free space in `/tmp`.
///
/// Metric type: Gauge
/// Unit: bytes
pub const TMP_FREE_METRIC: &str = "aws.lambda.enhanced.tmp_free";

// Resource Metrics

/// Maximum file descriptors available (ulimit -n).
///
/// Metric type: Gauge
/// Unit: count
pub const FD_MAX_METRIC: &str = "aws.lambda.enhanced.fd_max";

/// Current number of open file descriptors.
///
/// Metric type: Gauge
/// Unit: count
pub const FD_USE_METRIC: &str = "aws.lambda.enhanced.fd_use";

/// Maximum threads available (ulimit -u).
///
/// Metric type: Gauge
/// Unit: count
pub const THREADS_MAX_METRIC: &str = "aws.lambda.enhanced.threads_max";

/// Current number of threads.
///
/// Metric type: Gauge
/// Unit: count
pub const THREADS_USE_METRIC: &str = "aws.lambda.enhanced.threads_use";

// Commented out for general agent build
// /// AppSec-enabled invocations count.
// ///
// /// Tracks invocations where Application Security is active.
// ///
// /// Metric type: Count
// pub const ASM_INVOCATIONS_METRIC: &str = "aws.lambda.enhanced.asm.invocations";

// Configuration

/// Environment variable to enable/disable enhanced metrics.
///
/// Set to "true" or "false". Default: "true"
pub const ENHANCED_METRICS_ENV_VAR: &str = "DD_ENHANCED_METRICS";
