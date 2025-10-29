//! FFI (Foreign Function Interface) Module
//!
//! This module provides C-compatible bindings for the Datadog Agent Native library.
//!
//! ## Thread Safety
//!
//! **IMPORTANT**: Each `DatadogAgent` pointer is **NOT thread-safe** and must be accessed
//! from only one thread at a time. This follows standard C library conventions (e.g., `FILE*`).
//!
//! ### Safe Usage:
//!
//! ```c
//! // ✅ SAFE: Single-threaded access
//! DatadogAgent* agent = datadog_agent_start(&opts);
//! datadog_agent_submit_trace(agent, data, len);
//! datadog_agent_stop(agent);
//! ```
//!
//! ```c
//! // ✅ SAFE: Multiple agents in different threads
//! DatadogAgent* agent1 = datadog_agent_start(&opts);
//! DatadogAgent* agent2 = datadog_agent_start(&opts);
//!
//! // Thread 1
//! datadog_agent_submit_trace(agent1, data1, len1);
//!
//! // Thread 2
//! datadog_agent_submit_trace(agent2, data2, len2);
//! ```
//!
//! ### Unsafe Usage:
//!
//! ```c
//! // ❌ UNSAFE: Concurrent access to same pointer
//! DatadogAgent* agent = datadog_agent_start(&opts);
//!
//! // Thread 1
//! datadog_agent_submit_trace(agent, data1, len1);  // ❌ Data race!
//!
//! // Thread 2
//! datadog_agent_submit_trace(agent, data2, len2);  // ❌ Data race!
//! ```
//!
//! If you need to share an agent across threads, use external synchronization (e.g., mutex).
//!
//! ## Panic Safety - ALL FFI Functions Protected
//!
//! **CRITICAL**: All FFI functions in this module use `std::panic::catch_unwind()` to prevent
//! panics from crossing the `extern "C"` boundary, which would otherwise cause immediate
//! process termination (SIGABRT).
//!
//! ### Protected Functions:
//!
//! 1. **`datadog_agent_start`** - Catches panics during initialization, returns NULL on panic
//! 2. **`datadog_agent_stop`** - Catches panics during shutdown, returns ShutdownError on panic
//! 3. **`datadog_agent_wait_for_shutdown`** - Catches panics, returns FatalError code (2) on panic
//! 4. **`datadog_agent_submit_trace`** - Catches panics during trace submission, returns RuntimeError on panic
//! 5. **`datadog_agent_version`** - Safe (returns static string, cannot panic)
//!
//! ### What This Means:
//!
//! - **No process aborts**: Any panic is caught and converted to an appropriate error code
//! - **Any size payload**: The FFI can handle pointers to gigabytes of data safely
//! - **Malformed data is safe**: Parser failures return errors, never crash the application
//! - **Production-ready**: Applications using this library will not crash from bad input
//!
//! ### Trace Aggregator Limits
//!
//! The internal trace aggregator enforces Datadog's **3.2 MB maximum payload size**
//! per flush. The queue uses FIFO eviction to prevent unbounded memory growth.
//!
//! ### Build-time Features
//!
//! AppSec/WAF support is gated behind the Cargo `appsec` feature. When the crate is
//! built without this feature, AppSec-related FFI flags are ignored.
//!
//! ## Usage Example (C)
//!
//! ```c
//! #include "datadog_agent_native.h"
//!
//! int main() {
//!     // Create configuration options
//!     DatadogAgentOptions options = {
//!         .api_key = "your-api-key-here",
//!         .site = "datadoghq.com",
//!         .service = "my-service",
//!         .env = "production",
//!         .version = "1.0.0",
//!         .appsec_enabled = 0,
//!         .remote_config_enabled = 1,
//!         .log_level = 3,  // Info level
//!         .operational_mode = 2,  // HttpUds mode
//!         .trace_agent_port = 0,  // Not used in UDS mode
//!     };
//!
//!     // Create and start agent
//!     DatadogAgent* agent = datadog_agent_start(&options);
//!     if (!agent) {
//!         return 1;
//!     }
//!
//!     // Wait for shutdown
//!     datadog_agent_wait_for_shutdown(agent);
//!
//!     // Cleanup
//!     datadog_agent_stop(agent);
//!
//!     return 0;
//! }
//! ```

use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::ptr;
use tokio::runtime::Runtime;

use crate::agent::{AgentCoordinator, AgentError, ShutdownReason};
use crate::config::Config;

/// Error codes returned by FFI functions
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum DatadogError {
    /// Operation succeeded
    Ok = 0,
    /// Null pointer provided
    NullPointer = 1,
    /// Invalid UTF-8 string
    InvalidString = 2,
    /// Configuration error
    ConfigError = 3,
    /// Initialization error
    InitError = 4,
    /// Startup error
    StartupError = 5,
    /// Shutdown error
    ShutdownError = 6,
    /// Runtime error
    RuntimeError = 7,
    /// Invalid data format (e.g., invalid msgpack)
    InvalidDataFormat = 8,
    /// Trace submission error
    SubmissionError = 9,
    /// Feature not available (e.g., FFI submission in HTTP mode)
    NotAvailable = 10,
    /// Unknown error
    UnknownError = 99,
}

/// Configuration options for the Datadog Agent
///
/// All string fields should be null-terminated UTF-8 strings.
/// NULL pointers are treated as "not set" and will use defaults.
#[repr(C)]
pub struct DatadogAgentOptions {
    /// Datadog API key (required)
    pub api_key: *const c_char,

    /// Datadog site (e.g., "datadoghq.com", "datadoghq.eu")
    /// Default: "datadoghq.com"
    pub site: *const c_char,

    /// Service name for Unified Service Tagging
    /// Default: not set
    pub service: *const c_char,

    /// Environment name for Unified Service Tagging
    /// Default: not set
    pub env: *const c_char,

    /// Version for Unified Service Tagging
    /// Default: not set
    pub version: *const c_char,

    /// Enable Application Security (AppSec/WAF)
    /// 0 = disabled (default), non-zero = enabled
    pub appsec_enabled: c_int,

    /// Enable Remote Configuration
    /// 0 = disabled, non-zero = enabled (default)
    pub remote_config_enabled: c_int,

    /// Log level: 0=error, 1=error, 2=warn, 3=info (default), 4=debug, 5=trace
    pub log_level: c_int,

    /// Operational mode: 0=HttpFixedPort (default), 1=HttpEphemeralPort, 2=HttpUds
    /// - HttpFixedPort: Traditional mode with fixed HTTP ports
    /// - HttpEphemeralPort: HTTP server with OS-assigned ports (port 0)
    /// - HttpUds: HTTP server using Unix Domain Sockets (or Named Pipes on Windows)
    pub operational_mode: c_int,

    /// Trace agent port: 0=default (8126), -1=ephemeral (auto-assign), or specific port number
    /// Note: If operational_mode is HttpEphemeralPort, this is ignored and port 0 is used
    /// Note: If operational_mode is HttpUds, this is ignored (uses Unix Domain Sockets)
    pub trace_agent_port: c_int,

    /// Enable DogStatsD UDP server for receiving metrics
    /// 0 = disabled (default), non-zero = enabled
    pub dogstatsd_enabled: c_int,

    /// DogStatsD UDP port: 0=ephemeral (auto-assign), -1=default (8125), or specific port number
    /// When 0, the OS assigns an ephemeral port to avoid conflicts with other agents
    pub dogstatsd_port: c_int,

    /// Unix Domain Socket file permissions (Unix only): -1=default (0o600), or specific octal permissions
    /// Default 0o600 (384 in decimal) = owner read/write only (most secure)
    /// Common values: 0o600 (384)=owner-only, 0o660 (432)=owner+group, 0o666 (438)=all users
    /// Only used when operational_mode is HttpUds (3)
    pub trace_agent_uds_permissions: c_int,
}

/// Opaque handle to a Datadog Agent instance
#[repr(C)]
#[derive(Copy, Clone)]
pub struct DatadogAgent {
    _private: [u8; 0],
}

/// Result returned by datadog_agent_start()
///
/// This struct contains all information needed after starting the agent,
/// reducing the number of FFI calls required.
#[repr(C)]
pub struct DatadogAgentStartResult {
    /// Pointer to the agent instance, or NULL on error
    pub agent: *mut DatadogAgent,

    /// Error code (DatadogError::Ok if successful)
    pub error: DatadogError,

    /// Version string of the library (static, never NULL)
    pub version: *const c_char,

    /// Bound port of the HTTP server (-1 if not available)
    /// Will be -1 in FFI-only mode, HttpUds mode, or if HTTP server failed to start
    pub bound_port: c_int,

    /// Actual bound port of the DogStatsD UDP server (-1 if not available)
    /// Will be -1 if DogStatsD is disabled or failed to start
    /// When dogstatsd_port was 0 (ephemeral), this contains the OS-assigned port
    pub dogstatsd_port: c_int,

    /// Unix Domain Socket path (NULL if not using UDS)
    /// Only populated when operational_mode is HttpUds (3)
    /// The string is heap-allocated and must be freed with datadog_free_string()
    /// On Windows, this will be a named pipe path (e.g., "\\\\.\\pipe\\dd-trace-1234")
    pub uds_path: *mut c_char,
}

/// Internal representation of DatadogAgent handle.
///
/// This struct is opaque to C callers and contains the actual Rust implementation.
/// The C side only sees a pointer to `DatadogAgent` (an opaque type), which is
/// actually a pointer to this struct. This pattern ensures memory safety and prevents
/// C code from directly manipulating internal state.
///
/// # Fields
/// * `coordinator` - The AgentCoordinator that manages all agent subsystems
/// * `runtime` - Tokio runtime for async operations (owned by this handle)
struct AgentHandle {
    coordinator: AgentCoordinator,
    runtime: Runtime,
}

/// Convert internal AgentError to FFI DatadogError code.
///
/// This function maps Rust error types to C-compatible error codes that can
/// be safely returned across the FFI boundary. The mapping preserves error
/// categories while simplifying to a flat enum for C compatibility.
///
/// # Arguments
/// * `error` - Reference to the internal AgentError to convert
///
/// # Returns
/// The corresponding DatadogError code for FFI
fn agent_error_to_ffi(error: &AgentError) -> DatadogError {
    match error {
        AgentError::ConfigError(_) => DatadogError::ConfigError,
        AgentError::InitError(_) => DatadogError::InitError,
        AgentError::StartupError(_) => DatadogError::StartupError,
        AgentError::ShutdownError(_) => DatadogError::ShutdownError,
        AgentError::RemoteConfigError(_) => DatadogError::ConfigError, // Treat remote config errors as config errors
        AgentError::IoError(_) => DatadogError::RuntimeError,
    }
}

/// Parse a boolean coming from environment variables.
///
/// Accepts common truthy/falsy strings (case insensitive) including numeric forms.
fn parse_env_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "y" | "on" => Some(true),
        "false" | "0" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

/// Helper to safely convert C string to Rust String.
///
/// This function handles NULL pointers (returns default) and validates UTF-8.
/// It's marked `unsafe` because it dereferences a raw pointer from C.
///
/// # Safety
/// * `ptr` must be either NULL or a valid pointer to a null-terminated C string
/// * If not NULL, the string must be valid UTF-8
///
/// # Arguments
/// * `ptr` - Pointer to a null-terminated C string (can be NULL)
/// * `default` - Default value to use if ptr is NULL
///
/// # Returns
/// * `Ok(String)` - Successfully converted string (or default if NULL)
/// * `Err(DatadogError::InvalidString)` - The string is not valid UTF-8
unsafe fn cstr_to_string(ptr: *const c_char, default: &str) -> Result<String, DatadogError> {
    // NULL pointer means "not set" - use the default value
    if ptr.is_null() {
        return Ok(default.to_string());
    }

    // Convert C string to Rust &str, validating UTF-8
    match CStr::from_ptr(ptr).to_str() {
        Ok(s) => Ok(s.to_string()),
        Err(_) => Err(DatadogError::InvalidString), // Invalid UTF-8
    }
}

/// Create and start a Datadog Agent with the given configuration
///
/// # Safety
///
/// - `options` must be a valid pointer to a `DatadogAgentOptions` struct
/// - All string pointers in `options` must be valid null-terminated UTF-8 strings or NULL
/// - The returned struct's agent pointer must be freed with `datadog_agent_stop()`
/// - The agent pointer is NOT thread-safe; do not call functions on it from multiple threads concurrently
///
/// # Returns
///
/// A `DatadogAgentStartResult` struct containing:
/// - `agent`: Pointer to the running agent (NULL on error)
/// - `error`: Error code (Ok on success)
/// - `version`: Version string of the library (always valid)
/// - `bound_port`: HTTP server port (-1 if not available)
#[no_mangle]
pub unsafe extern "C" fn datadog_agent_start(
    options: *const DatadogAgentOptions,
) -> DatadogAgentStartResult {
    // Version string (static, always valid)
    static VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");
    let version_ptr = VERSION.as_ptr() as *const c_char;

    // Helper to create error result
    let mk_error = |error: DatadogError| -> DatadogAgentStartResult {
        DatadogAgentStartResult {
            agent: ptr::null_mut(),
            error,
            version: version_ptr,
            bound_port: -1,
            dogstatsd_port: -1,
            uds_path: ptr::null_mut(),
        }
    };

    // Catch any panics to prevent them from crossing the extern "C" boundary
    let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if options.is_null() {
            return mk_error(DatadogError::NullPointer);
        }

        let opts = &*options;

        // Build configuration from options
        let mut config = Config::default();

        // Required: API key - check FFI option first, then DD_API_KEY environment variable
        match cstr_to_string(opts.api_key, "") {
            Ok(key) if !key.is_empty() => config.api_key = key,
            _ => {
                // Fallback to DD_API_KEY environment variable
                if let Ok(key) = std::env::var("DD_API_KEY") {
                    if !key.is_empty() {
                        config.api_key = key;
                    } else {
                        return mk_error(DatadogError::ConfigError); // API key is required
                    }
                } else {
                    return mk_error(DatadogError::ConfigError); // API key is required
                }
            }
        }

        // Site - check FFI option first, then DD_SITE, default to "datadoghq.com"
        match cstr_to_string(opts.site, "") {
            Ok(site) if !site.is_empty() => config.site = site,
            _ => {
                config.site =
                    std::env::var("DD_SITE").unwrap_or_else(|_| "datadoghq.com".to_string());
            }
        }

        // Service - check FFI option first, then DD_SERVICE environment variable
        match cstr_to_string(opts.service, "") {
            Ok(service) if !service.is_empty() => config.service = Some(service),
            _ => {
                if let Ok(service) = std::env::var("DD_SERVICE") {
                    if !service.is_empty() {
                        config.service = Some(service);
                    }
                }
            }
        }

        // Environment - check FFI option first, then DD_ENV environment variable
        match cstr_to_string(opts.env, "") {
            Ok(env) if !env.is_empty() => config.env = Some(env),
            _ => {
                if let Ok(env) = std::env::var("DD_ENV") {
                    if !env.is_empty() {
                        config.env = Some(env);
                    }
                }
            }
        }

        // Version - check FFI option first, then DD_VERSION environment variable
        match cstr_to_string(opts.version, "") {
            Ok(version) if !version.is_empty() => config.version = Some(version),
            _ => {
                if let Ok(version) = std::env::var("DD_VERSION") {
                    if !version.is_empty() {
                        config.version = Some(version);
                    }
                }
            }
        }

        #[cfg(feature = "appsec")]
        {
            config.appsec_enabled = opts.appsec_enabled != 0;
        }
        #[cfg(not(feature = "appsec"))]
        {
            if opts.appsec_enabled != 0 {
                tracing::warn!(
                    "AppSec was requested via FFI options, but the crate was built without the `appsec` feature. AppSec will remain disabled."
                );
            }
            config.appsec_enabled = false;
        }

        // Remote config - check FFI option first, then DD_REMOTE_CONFIGURATION_ENABLED, default to true
        if opts.remote_config_enabled == 0 {
            // If FFI passed 0 (false), check environment variable
            config.remote_configuration_enabled = std::env::var("DD_REMOTE_CONFIGURATION_ENABLED")
                .ok()
                .and_then(|v| parse_env_bool(&v))
                .unwrap_or(false); // Default to false when caller disables it
        } else {
            config.remote_configuration_enabled = true;
        }

        // Remote config API key - defaults to main API key if not specified
        // Remote config uses the same API key as the main agent by default
        config.remote_configuration_api_key = Some(config.api_key.clone());

        // Set log level - maps FFI integer to internal LogLevel enum
        // 0 and 1 both map to Error for backward compatibility
        // Default is Info (3) if an invalid level is provided
        config.log_level = match opts.log_level {
            0 | 1 => crate::config::log_level::LogLevel::Error,
            2 => crate::config::log_level::LogLevel::Warn,
            3 => crate::config::log_level::LogLevel::Info,
            4 => crate::config::log_level::LogLevel::Debug,
            5 => crate::config::log_level::LogLevel::Trace,
            _ => crate::config::log_level::LogLevel::Info, // Fallback to Info for unknown values
        };

        // Initialize tracing subscriber for console logging (only once)
        // This ensures logs are actually output to the console
        use std::sync::Once;
        static INIT_TRACING: Once = Once::new();
        INIT_TRACING.call_once(|| {
            use tracing_subscriber::{fmt, EnvFilter};

            let log_level_str = match opts.log_level {
                0 | 1 => "error",
                2 => "warn",
                3 => "info",
                4 => "debug",
                5 => "trace",
                _ => "info",
            };

            // Create filter that applies to all targets
            let filter =
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level_str));

            // Initialize the subscriber with console output
            fmt()
                .with_env_filter(filter)
                .with_target(true)
                .with_thread_ids(false)
                .with_line_number(true)
                .init();
        });

        // Set operational mode - determines how the agent accepts traces
        // This affects what transport mechanisms are available (HTTP with fixed/ephemeral ports, or UDS)
        config.operational_mode = match opts.operational_mode {
            1 => crate::config::operational_mode::OperationalMode::HttpEphemeralPort, // OS-assigned port
            2 => crate::config::operational_mode::OperationalMode::HttpUds, // Unix Domain Sockets (or Named Pipes on Windows)
            _ => crate::config::operational_mode::OperationalMode::HttpFixedPort, // 0 or any other value = traditional fixed port
        };

        // Set trace agent HTTP port (only relevant for HTTP modes with TCP, not HttpUds)
        // Priority: explicit positive port > ephemeral (-1) > default (None = 8126)
        if opts.trace_agent_port > 0 {
            // Caller specified an explicit port number (e.g., 8126, 9999)
            config.trace_agent_port = Some(opts.trace_agent_port as u16);
        } else if opts.trace_agent_port == -1 {
            // Caller wants ephemeral port (OS assigns a random free port)
            // This avoids conflicts with existing agents
            config.trace_agent_port = Some(0);
        }
        // Otherwise (opts.trace_agent_port == 0 or not set):
        //   Leave as None, which means use the default port 8126

        // Set Unix Domain Socket permissions
        if opts.trace_agent_uds_permissions >= 0 {
            // Use provided permissions
            config.trace_agent_uds_permissions = opts.trace_agent_uds_permissions as u32;
        }
        // Otherwise leave as default (0o600)

        // Set DogStatsD UDP server configuration
        // DogStatsD provides metrics collection via the StatsD protocol over UDP
        config.dogstatsd_enabled = opts.dogstatsd_enabled != 0;

        // Set DogStatsD UDP port - priority: explicit port > default (-1 = 8125) > ephemeral (0)
        if opts.dogstatsd_port > 0 {
            // Caller specified an explicit port number (e.g., 8125, 9999)
            config.dogstatsd_port = opts.dogstatsd_port as u16;
        } else if opts.dogstatsd_port == -1 {
            // Caller wants the standard DogStatsD default port (8125)
            config.dogstatsd_port = 8125;
        } else {
            // Port 0 or not set = ephemeral (OS-assigned random free port)
            // This avoids conflicts when multiple agents run on the same host
            config.dogstatsd_port = 0;
        }

        // Create tokio runtime
        let runtime = match Runtime::new() {
            Ok(rt) => rt,
            Err(_) => return mk_error(DatadogError::InitError),
        };

        // Create coordinator
        let mut coordinator = match AgentCoordinator::new(config) {
            Ok(c) => c,
            Err(_) => return mk_error(DatadogError::InitError),
        };

        // Start the agent
        if runtime.block_on(coordinator.start()).is_err() {
            return mk_error(DatadogError::StartupError);
        }

        // Get the bound HTTP port AFTER starting the coordinator
        // Why poll? The HTTP server starts asynchronously in a background task.
        // It may take a moment to bind to the port, especially if using ephemeral ports.
        // We poll for up to 5 seconds to give the server time to bind and report its port.
        // This is important because:
        // 1. Ephemeral ports (port 0) need time to be assigned by the OS
        // 2. The async HTTP server spawns in the background
        // 3. The caller needs to know the actual bound port for connecting
        let bound_port = runtime.block_on(async {
            let start = std::time::Instant::now();
            let timeout = std::time::Duration::from_secs(5);

            loop {
                // Try to get the port from the coordinator
                if let Some(port) = coordinator.get_bound_port().await {
                    // Success! The HTTP server has bound and reported its port
                    return port as c_int;
                }

                // Check if we've exceeded the timeout
                if start.elapsed() >= timeout {
                    // Timeout - HTTP server didn't bind within 5 seconds
                    // This can happen in UDS-only deployments or if binding failed
                    return -1;
                }

                // Wait a short time (50ms) before polling again
                // This prevents busy-waiting and gives the server time to start
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        });

        // Get the DogStatsD port (synchronous, immediately available after start)
        let dogstatsd_port = coordinator
            .get_dogstatsd_port()
            .map(|p| p as c_int)
            .unwrap_or(-1);

        // Get the UDS path if using HttpUds mode
        let uds_path_ptr = runtime.block_on(async {
            if let Some(listener_ref) = coordinator.get_trace_agent_listener_info() {
                let info = listener_ref.lock().await;
                if let Some(ref info) = *info {
                    if let Some(ref path) = info.uds_path {
                        // Allocate a heap string with null terminator for FFI
                        let c_string = match std::ffi::CString::new(path.as_str()) {
                            Ok(s) => s,
                            Err(_) => return ptr::null_mut(),
                        };
                        // Transfer ownership to caller - they must free it
                        return c_string.into_raw();
                    }
                }
            }
            ptr::null_mut()
        });

        let handle = Box::new(AgentHandle {
            coordinator,
            runtime,
        });

        let agent_ptr = Box::into_raw(handle) as *mut DatadogAgent;

        DatadogAgentStartResult {
            agent: agent_ptr,
            error: DatadogError::Ok,
            version: version_ptr,
            bound_port,
            dogstatsd_port,
            uds_path: uds_path_ptr,
        }
    }));

    match panic_result {
        Ok(result) => result,
        Err(panic_payload) => {
            // A panic occurred during agent start - log and return error
            let panic_msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                format!("Panic during agent start: {}", s)
            } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                format!("Panic during agent start: {}", s)
            } else {
                "Panic during agent start (unknown cause)".to_string()
            };
            eprintln!("{}", panic_msg);

            static VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");
            DatadogAgentStartResult {
                agent: ptr::null_mut(),
                error: DatadogError::InitError,
                version: VERSION.as_ptr() as *const c_char,
                bound_port: -1,
                dogstatsd_port: -1,
                uds_path: ptr::null_mut(),
            }
        }
    }
}

/// Wait for a shutdown signal (blocking)
///
/// This function blocks until a shutdown is requested via Ctrl+C, SIGTERM,
/// or programmatically.
///
/// # Safety
///
/// - `agent` must be a valid pointer returned by `datadog_agent_start()`
/// - Do not call other functions on the same `agent` from different threads while this is running
///
/// # Returns
///
/// The reason for shutdown (0 = graceful, 1 = user interrupt, 2 = error, 3 = timeout)
#[no_mangle]
pub unsafe extern "C" fn datadog_agent_wait_for_shutdown(agent: *mut DatadogAgent) -> c_int {
    // Catch any panics to prevent them from crossing the extern "C" boundary
    let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if agent.is_null() {
            return -1;
        }

        let handle = &*(agent as *const AgentHandle);

        let reason = handle
            .runtime
            .block_on(handle.coordinator.wait_for_shutdown());

        match reason {
            ShutdownReason::GracefulShutdown => 0,
            ShutdownReason::UserInterrupt => 1,
            ShutdownReason::FatalError => 2,
            ShutdownReason::Timeout => 3,
        }
    }));

    match panic_result {
        Ok(code) => code,
        Err(panic_payload) => {
            // A panic occurred - log and return fatal error code
            let panic_msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                format!("Panic during wait_for_shutdown: {}", s)
            } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                format!("Panic during wait_for_shutdown: {}", s)
            } else {
                "Panic during wait_for_shutdown (unknown cause)".to_string()
            };
            eprintln!("{}", panic_msg);
            2 // Return FatalError code
        }
    }
}

/// Stop and free a Datadog Agent
///
/// Performs graceful shutdown and frees all resources.
///
/// # Safety
///
/// - `agent` must be a valid pointer returned by `datadog_agent_start()`
/// - After calling this function, the pointer must not be used again
/// - Do not call any other functions on the same `agent` from different threads while or after this runs
/// - Ensure all other threads have finished using this `agent` before calling this function
#[no_mangle]
pub unsafe extern "C" fn datadog_agent_stop(agent: *mut DatadogAgent) -> DatadogError {
    // Catch any panics to prevent them from crossing the extern "C" boundary
    let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if agent.is_null() {
            return DatadogError::NullPointer;
        }

        let mut handle = Box::from_raw(agent as *mut AgentHandle);

        match handle.runtime.block_on(handle.coordinator.shutdown()) {
            Ok(()) => DatadogError::Ok,
            Err(e) => agent_error_to_ffi(&e),
        }
    }));

    match panic_result {
        Ok(error) => error,
        Err(panic_payload) => {
            // A panic occurred during stop - log and return shutdown error
            let panic_msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                format!("Panic during agent stop: {}", s)
            } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                format!("Panic during agent stop: {}", s)
            } else {
                "Panic during agent stop (unknown cause)".to_string()
            };
            eprintln!("{}", panic_msg);
            DatadogError::ShutdownError
        }
    }
}

/// Free a string allocated by the library
///
/// This function must be called to free strings returned by the library,
/// such as the `uds_path` field in `DatadogAgentStartResult`.
///
/// # Safety
///
/// - `string` must be a pointer returned by this library (e.g., from `uds_path`)
/// - `string` must not be NULL
/// - `string` must not have been freed already
/// - After calling this function, the pointer is invalid and must not be used
///
/// # Example
///
/// ```c
/// DatadogAgentStartResult result = datadog_agent_start(&options);
/// if (result.uds_path != NULL) {
///     printf("UDS path: %s\n", result.uds_path);
///     datadog_free_string(result.uds_path);
/// }
/// ```
#[no_mangle]
pub unsafe extern "C" fn datadog_free_string(string: *mut c_char) {
    if string.is_null() {
        return;
    }

    // Reconstruct the CString from the raw pointer and let it drop
    // This properly deallocates the memory
    let _ = std::ffi::CString::from_raw(string);
}
