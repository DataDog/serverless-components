#ifndef DATADOG_AGENT_NATIVE_H
#define DATADOG_AGENT_NATIVE_H

#pragma once

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

/**
 * Error codes returned by FFI functions
 */
typedef enum DatadogError {
  /**
   * Operation succeeded
   */
  Ok = 0,
  /**
   * Null pointer provided
   */
  NullPointer = 1,
  /**
   * Invalid UTF-8 string
   */
  InvalidString = 2,
  /**
   * Configuration error
   */
  ConfigError = 3,
  /**
   * Initialization error
   */
  InitError = 4,
  /**
   * Startup error
   */
  StartupError = 5,
  /**
   * Shutdown error
   */
  ShutdownError = 6,
  /**
   * Runtime error
   */
  RuntimeError = 7,
  /**
   * Invalid data format (e.g., invalid msgpack)
   */
  InvalidDataFormat = 8,
  /**
   * Trace submission error
   */
  SubmissionError = 9,
  /**
   * Feature not available (e.g., FFI submission in HTTP mode)
   */
  NotAvailable = 10,
  /**
   * Unknown error
   */
  UnknownError = 99,
} DatadogError;

/**
 * Opaque handle to a Datadog Agent instance
 */
typedef struct DatadogAgent {
  uint8_t _private[0];
} DatadogAgent;

/**
 * Result returned by datadog_agent_start()
 *
 * This struct contains all information needed after starting the agent,
 * reducing the number of FFI calls required.
 */
typedef struct DatadogAgentStartResult {
  /**
   * Pointer to the agent instance, or NULL on error
   */
  struct DatadogAgent *agent;
  /**
   * Error code (DatadogError::Ok if successful)
   */
  enum DatadogError error;
  /**
   * Version string of the library (static, never NULL)
   */
  const char *version;
  /**
   * Bound port of the HTTP server (-1 if not available)
   * Will be -1 in FFI-only mode, HttpUds mode, or if HTTP server failed to start
   */
  int bound_port;
  /**
   * Actual bound port of the DogStatsD UDP server (-1 if not available)
   * Will be -1 if DogStatsD is disabled or failed to start
   * When dogstatsd_port was 0 (ephemeral), this contains the OS-assigned port
   */
  int dogstatsd_port;
  /**
   * Unix Domain Socket path (NULL if not using UDS)
   * Only populated when operational_mode is HttpUds (3)
   * The string is heap-allocated and must be freed with datadog_free_string()
   * On Windows, this will be a named pipe path (e.g., "\\\\.\\pipe\\dd-trace-1234")
   */
  char *uds_path;
} DatadogAgentStartResult;

/**
 * Configuration options for the Datadog Agent
 *
 * All string fields should be null-terminated UTF-8 strings.
 * NULL pointers are treated as "not set" and will use defaults.
 */
typedef struct DatadogAgentOptions {
  /**
   * Datadog API key (required)
   */
  const char *api_key;
  /**
   * Datadog site (e.g., "datadoghq.com", "datadoghq.eu")
   * Default: "datadoghq.com"
   */
  const char *site;
  /**
   * Service name for Unified Service Tagging
   * Default: not set
   */
  const char *service;
  /**
   * Environment name for Unified Service Tagging
   * Default: not set
   */
  const char *env;
  /**
   * Version for Unified Service Tagging
   * Default: not set
   */
  const char *version;
  /**
   * Enable Application Security (AppSec/WAF)
   * 0 = disabled (default), non-zero = enabled
   */
  int appsec_enabled;
  /**
   * Enable Remote Configuration
   * 0 = disabled, non-zero = enabled (default)
   */
  int remote_config_enabled;
  /**
   * Log level: 0=error, 1=error, 2=warn, 3=info (default), 4=debug, 5=trace
   */
  int log_level;
  /**
   * Operational mode: 0=HttpFixedPort (default), 1=HttpEphemeralPort, 2=HttpUds
   * - HttpFixedPort: Traditional mode with fixed HTTP ports
   * - HttpEphemeralPort: HTTP server with OS-assigned ports (port 0)
   * - HttpUds: HTTP server using Unix Domain Sockets (or Named Pipes on Windows)
   */
  int operational_mode;
  /**
   * Trace agent port: 0=default (8126), -1=ephemeral (auto-assign), or specific port number
   * Note: If operational_mode is HttpEphemeralPort, this is ignored and port 0 is used
   * Note: If operational_mode is HttpUds, this is ignored (uses Unix Domain Sockets)
   */
  int trace_agent_port;
  /**
   * Enable DogStatsD UDP server for receiving metrics
   * 0 = disabled (default), non-zero = enabled
   */
  int dogstatsd_enabled;
  /**
   * DogStatsD UDP port: 0=ephemeral (auto-assign), -1=default (8125), or specific port number
   * When 0, the OS assigns an ephemeral port to avoid conflicts with other agents
   */
  int dogstatsd_port;
  /**
   * Unix Domain Socket file permissions (Unix only): -1=default (0o600), or specific octal permissions
   * Default 0o600 (384 in decimal) = owner read/write only (most secure)
   * Common values: 0o600 (384)=owner-only, 0o660 (432)=owner+group, 0o666 (438)=all users
   * Only used when operational_mode is HttpUds (3)
   */
  int trace_agent_uds_permissions;
} DatadogAgentOptions;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * Create and start a Datadog Agent with the given configuration
 *
 * # Safety
 *
 * - `options` must be a valid pointer to a `DatadogAgentOptions` struct
 * - All string pointers in `options` must be valid null-terminated UTF-8 strings or NULL
 * - The returned struct's agent pointer must be freed with `datadog_agent_stop()`
 * - The agent pointer is NOT thread-safe; do not call functions on it from multiple threads concurrently
 *
 * # Returns
 *
 * A `DatadogAgentStartResult` struct containing:
 * - `agent`: Pointer to the running agent (NULL on error)
 * - `error`: Error code (Ok on success)
 * - `version`: Version string of the library (always valid)
 * - `bound_port`: HTTP server port (-1 if not available)
 */
struct DatadogAgentStartResult datadog_agent_start(const struct DatadogAgentOptions *options);

/**
 * Wait for a shutdown signal (blocking)
 *
 * This function blocks until a shutdown is requested via Ctrl+C, SIGTERM,
 * or programmatically.
 *
 * # Safety
 *
 * - `agent` must be a valid pointer returned by `datadog_agent_start()`
 * - Do not call other functions on the same `agent` from different threads while this is running
 *
 * # Returns
 *
 * The reason for shutdown (0 = graceful, 1 = user interrupt, 2 = error, 3 = timeout)
 */
int datadog_agent_wait_for_shutdown(struct DatadogAgent *agent);

/**
 * Stop and free a Datadog Agent
 *
 * Performs graceful shutdown and frees all resources.
 *
 * # Safety
 *
 * - `agent` must be a valid pointer returned by `datadog_agent_start()`
 * - After calling this function, the pointer must not be used again
 * - Do not call any other functions on the same `agent` from different threads while or after this runs
 * - Ensure all other threads have finished using this `agent` before calling this function
 */
enum DatadogError datadog_agent_stop(struct DatadogAgent *agent);

/**
 * Free a string allocated by the library
 *
 * This function must be called to free strings returned by the library,
 * such as the `uds_path` field in `DatadogAgentStartResult`.
 *
 * # Safety
 *
 * - `string` must be a pointer returned by this library (e.g., from `uds_path`)
 * - `string` must not be NULL
 * - `string` must not have been freed already
 * - After calling this function, the pointer is invalid and must not be used
 *
 * # Example
 *
 * ```c
 * DatadogAgentStartResult result = datadog_agent_start(&options);
 * if (result.uds_path != NULL) {
 *     printf("UDS path: %s\n", result.uds_path);
 *     datadog_free_string(result.uds_path);
 * }
 * ```
 */
void datadog_free_string(char *string);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* DATADOG_AGENT_NATIVE_H */
