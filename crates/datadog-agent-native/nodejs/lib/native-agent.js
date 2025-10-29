// Unless explicitly stated otherwise all files in this repository are licensed under the Apache 2 License.
// This product includes software developed at Datadog (https://www.datadoghq.com/). Copyright 2017 Datadog, Inc.

/**
 * Node.js wrapper for the Datadog Agent Native library.
 *
 * This module provides a Node.js interface to the native Rust-based Datadog Agent
 * using ffi-napi for FFI (Foreign Function Interface).
 */

const ffi = require('ffi-napi');
const ref = require('ref-napi');
const Struct = require('ref-struct-di')(ref);
const path = require('path');
const os = require('os');

// Type definitions
const VoidPtr = ref.refType(ref.types.void);
const StringPtr = ref.refType(ref.types.CString);

// Enums
const LogLevel = {
  ERROR: 0,
  WARN: 2,
  INFO: 3,
  DEBUG: 4,
  TRACE: 5
};

const OperationalMode = {
  HTTP_FIXED_PORT: 0,      // Traditional mode with fixed HTTP ports (default 8126)
  HTTP_EPHEMERAL_PORT: 1,  // HTTP server with OS-assigned ports (port 0)
  HTTP_UDS: 2              // Unix Domain Socket mode (Unix only)
};

const ShutdownReason = {
  GRACEFUL_SHUTDOWN: 0,
  USER_INTERRUPT: 1,
  FATAL_ERROR: 2,
  TIMEOUT: 3
};

const DatadogError = {
  OK: 0,
  NULL_POINTER: 1,
  INVALID_STRING: 2,
  CONFIG_ERROR: 3,
  INIT_ERROR: 4,
  STARTUP_ERROR: 5,
  SHUTDOWN_ERROR: 6,
  RUNTIME_ERROR: 7,
  INVALID_DATA_FORMAT: 8,
  SUBMISSION_ERROR: 9,
  NOT_AVAILABLE: 10,
  UNKNOWN_ERROR: 99
};

// Native structures
const DatadogAgentOptions = Struct({
  api_key: StringPtr,
  site: StringPtr,
  service: StringPtr,
  env: StringPtr,
  version: StringPtr,
  appsec_enabled: ref.types.int,
  remote_config_enabled: ref.types.int,
  log_level: ref.types.int,
  operational_mode: ref.types.int,
  trace_agent_port: ref.types.int,
  dogstatsd_enabled: ref.types.int,
  dogstatsd_port: ref.types.int,
  trace_agent_uds_permissions: ref.types.int
});

const DatadogAgentStartResult = Struct({
  agent: VoidPtr,
  error: ref.types.int,
  version: StringPtr,
  bound_port: ref.types.int,
  dogstatsd_port: ref.types.int,
  uds_path: StringPtr
});

/**
 * Exception thrown by Datadog Agent operations.
 */
class DatadogAgentException extends Error {
  constructor(message) {
    super(message);
    this.name = 'DatadogAgentException';
  }
}

/**
 * Configuration for the Datadog Agent.
 */
class DatadogAgentConfig {
  /**
   * Create a new agent configuration.
   *
   * @param {Object} options - Configuration options
   * @param {string} [options.apiKey] - Datadog API key (if null, will be read from DD_API_KEY env var)
   * @param {string} [options.site] - Datadog site (e.g., "datadoghq.com", "datadoghq.eu")
   * @param {string} [options.service] - Service name for Unified Service Tagging
   * @param {string} [options.environment] - Environment name (e.g., "production", "staging")
   * @param {string} [options.version] - Version for Unified Service Tagging
   * @param {boolean} [options.appsecEnabled=false] - Enable Application Security (AppSec/WAF)
   * @param {boolean} [options.remoteConfigEnabled=true] - Enable Remote Configuration
   * @param {number} [options.logLevel=LogLevel.INFO] - Log level for the agent
   * @param {number} [options.operationalMode=OperationalMode.HTTP_EPHEMERAL_PORT] - Operational mode
   * @param {number} [options.traceAgentPort=0] - Trace agent port (null=default 8126, 0=ephemeral, >0=specific)
   * @param {boolean} [options.dogstatsdEnabled=false] - Enable DogStatsD UDP server
   * @param {number} [options.dogstatsdPort=0] - DogStatsD UDP port (null=default 8125, 0=ephemeral, >0=specific)
   * @param {number} [options.traceAgentUdsPermissions] - Unix Domain Socket permissions (Unix only)
   */
  constructor(options = {}) {
    this.apiKey = options.apiKey || null;
    this.site = options.site || null;
    this.service = options.service || null;
    this.environment = options.environment || null;
    this.version = options.version || null;
    this.appsecEnabled = options.appsecEnabled || false;
    this.remoteConfigEnabled = options.remoteConfigEnabled !== false; // Default true
    this.logLevel = options.logLevel !== undefined ? options.logLevel : LogLevel.INFO;
    this.operationalMode = options.operationalMode !== undefined ? options.operationalMode : OperationalMode.HTTP_EPHEMERAL_PORT;
    this.traceAgentPort = options.traceAgentPort !== undefined ? options.traceAgentPort : 0;
    this.dogstatsdEnabled = options.dogstatsdEnabled || false;
    this.dogstatsdPort = options.dogstatsdPort !== undefined ? options.dogstatsdPort : 0;
    this.traceAgentUdsPermissions = options.traceAgentUdsPermissions !== undefined ? options.traceAgentUdsPermissions : null;
  }

  /**
   * Convert to native FFI options structure.
   *
   * Note: The returned structure contains Buffer objects (created by ref.allocCString)
   * that hold the string data. The caller must keep the returned structure alive
   * during the FFI call to prevent garbage collection of these buffers.
   *
   * @returns {DatadogAgentOptions}
   */
  toNativeOptions() {
    const options = new DatadogAgentOptions();

    options.api_key = this.apiKey ? ref.allocCString(this.apiKey) : ref.NULL;
    options.site = this.site ? ref.allocCString(this.site) : ref.NULL;
    options.service = this.service ? ref.allocCString(this.service) : ref.NULL;
    options.env = this.environment ? ref.allocCString(this.environment) : ref.NULL;
    options.version = this.version ? ref.allocCString(this.version) : ref.NULL;
    options.appsec_enabled = this.appsecEnabled ? 1 : 0;
    options.remote_config_enabled = this.remoteConfigEnabled ? 1 : 0;
    options.log_level = this.logLevel;
    options.operational_mode = this.operationalMode;
    options.trace_agent_port = this.traceAgentPort !== null ? this.traceAgentPort : -1;
    options.dogstatsd_enabled = this.dogstatsdEnabled ? 1 : 0;
    options.dogstatsd_port = this.dogstatsdPort !== null ? this.dogstatsdPort : -1;
    options.trace_agent_uds_permissions = this.traceAgentUdsPermissions !== null ? this.traceAgentUdsPermissions : -1;

    return options;
  }
}

/**
 * Node.js wrapper for the Datadog Agent Native library.
 */
class NativeAgent {
  /**
   * Create a new NativeAgent instance.
   *
   * @param {DatadogAgentConfig} config - Configuration options for the agent
   * @throws {DatadogAgentException} If agent initialization fails
   */
  constructor(config) {
    if (!config) {
      throw new Error('config cannot be null');
    }

    // Load the native library if not already loaded
    if (!NativeAgent._lib) {
      NativeAgent._lib = NativeAgent._loadLibrary();
    }

    // Convert config to native options
    const nativeOptions = config.toNativeOptions();

    // Call native start function
    const result = NativeAgent._lib.datadog_agent_start(nativeOptions);

    // Check for errors
    if (result.error !== DatadogError.OK || ref.isNull(result.agent)) {
      const errorName = Object.keys(DatadogError).find(key => DatadogError[key] === result.error) || 'UNKNOWN_ERROR';
      throw new DatadogAgentException(
        `Failed to start Datadog agent: ${errorName}. Check API key and configuration.`
      );
    }

    this._agentHandle = result.agent;
    this._disposed = false;

    // Extract version string
    this.version = ref.isNull(result.version) ? 'unknown' : ref.readCString(result.version, 0);

    // Extract bound port
    this.boundPort = result.bound_port > 0 ? result.bound_port : null;

    // Extract DogStatsD port
    this.dogstatsdPort = result.dogstatsd_port > 0 ? result.dogstatsd_port : null;

    // Extract UDS path
    this.udsPath = ref.isNull(result.uds_path) ? null : ref.readCString(result.uds_path, 0);
  }

  /**
   * Load the native library.
   * @private
   * @returns {Object} FFI library interface
   */
  static _loadLibrary() {
    const platform = os.platform();
    let libName;

    if (platform === 'darwin') {
      libName = 'libdatadog_agent_native.dylib';
    } else if (platform === 'linux') {
      libName = 'libdatadog_agent_native.so';
    } else if (platform === 'win32') {
      libName = 'datadog_agent_native.dll';
    } else {
      throw new DatadogAgentException(`Unsupported platform: ${platform}`);
    }

    // Try to find the library
    // 1. Check in the same directory as this file
    // 2. Check in parent directory
    // 3. Check in system library path

    let libPath = path.join(__dirname, libName);
    if (!require('fs').existsSync(libPath)) {
      libPath = path.join(__dirname, '..', libName);
    }
    if (!require('fs').existsSync(libPath)) {
      libPath = libName; // Try system path
    }

    try {
      return ffi.Library(libPath, {
        'datadog_agent_start': [DatadogAgentStartResult, [DatadogAgentOptions]],
        'datadog_agent_stop': [ref.types.int, [VoidPtr]],
        'datadog_agent_wait_for_shutdown': [ref.types.int, [VoidPtr]]
      });
    } catch (error) {
      throw new DatadogAgentException(
        `Failed to load native library ${libName}: ${error.message}. ` +
        `Make sure the library is in the same directory or in your system's library path.`
      );
    }
  }

  /**
   * Wait for a shutdown signal (blocking).
   * Returns when Ctrl+C, SIGTERM, or programmatic shutdown is triggered.
   *
   * @returns {number} Shutdown reason code
   * @throws {DatadogAgentException} If agent is disposed
   */
  waitForShutdown() {
    this._checkDisposed();
    return NativeAgent._lib.datadog_agent_wait_for_shutdown(this._agentHandle);
  }

  /**
   * Stop the agent and free all resources.
   */
  stop() {
    if (this._disposed) {
      return;
    }

    if (this._agentHandle && !ref.isNull(this._agentHandle)) {
      NativeAgent._lib.datadog_agent_stop(this._agentHandle);
      this._agentHandle = null;
    }

    this._disposed = true;
  }

  /**
   * Check if the agent has been disposed.
   * @private
   * @throws {DatadogAgentException} If agent is disposed
   */
  _checkDisposed() {
    if (this._disposed) {
      throw new DatadogAgentException('NativeAgent has been disposed');
    }
  }
}

// Static property for the native library
NativeAgent._lib = null;

// Singleton instance support
let _instance = null;

/**
 * Create a singleton instance of NativeAgent.
 *
 * @param {DatadogAgentConfig} [config] - Configuration for the agent. If null, uses defaults.
 * @returns {NativeAgent|null} NativeAgent instance or null if creation fails
 */
function createInstance(config = null) {
  try {
    if (!config) {
      config = new DatadogAgentConfig({
        appsecEnabled: true,
        logLevel: LogLevel.DEBUG,
        dogstatsdEnabled: true
      });
    }

    _instance = new NativeAgent(config);

    console.log(`Native Agent Version: ${_instance.version}`);
    console.log(`Native Trace Agent Port: ${_instance.boundPort}`);
    console.log(`Native UDP Port: ${_instance.dogstatsdPort}`);
    if (_instance.udsPath) {
      console.log(`Native UDS Path: ${_instance.udsPath}`);
    }

    return _instance;
  } catch (error) {
    console.error(`Native Agent Error: ${error.message}`);
    return null;
  }
}

/**
 * Get the singleton instance of NativeAgent.
 *
 * @returns {NativeAgent} The singleton NativeAgent instance
 * @throws {Error} If the instance has not been created
 */
function getInstance() {
  if (!_instance) {
    throw new Error('NativeAgent instance not initialized. Call createInstance() first.');
  }
  return _instance;
}

// Exports
module.exports = {
  NativeAgent,
  DatadogAgentConfig,
  DatadogAgentException,
  LogLevel,
  OperationalMode,
  ShutdownReason,
  DatadogError,
  createInstance,
  getInstance
};
