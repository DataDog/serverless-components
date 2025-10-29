//! FFI function tests for operational modes
//!
//! Tests the C FFI functions directly to ensure operational mode
//! configuration works correctly through the FFI interface.
//!
//! Note: Tests that use datadog_agent_submit_trace have been removed as
//! that functionality has been removed.

use std::ffi::CString;
use std::ptr;

// Import FFI types and functions
use datadog_agent_native::ffi::{
    datadog_agent_start, datadog_agent_stop, datadog_free_string, DatadogAgentOptions, DatadogError,
};
use tempfile::tempdir;

/// Test 1: FFI start with HttpFixedPort mode (operational_mode = 0)
#[test]
fn test_ffi_start_http_fixed_port_mode() {
    let api_key = CString::new("test-api-key-fixed").unwrap();
    let site = CString::new("datadoghq.com").unwrap();

    let options = DatadogAgentOptions {
        api_key: api_key.as_ptr(),
        site: site.as_ptr(),
        service: ptr::null(),
        env: ptr::null(),
        version: ptr::null(),
        appsec_enabled: 0,
        remote_config_enabled: 0,
        log_level: 1,
        operational_mode: 0,     // HttpFixedPort
        trace_agent_port: 18126, // Custom port to avoid conflicts
        dogstatsd_enabled: 0,
        dogstatsd_port: 8125,
        trace_agent_uds_permissions: -1,
    };

    unsafe {
        let result = datadog_agent_start(&options);
        assert_eq!(
            result.error,
            DatadogError::Ok,
            "Agent should start successfully in HttpFixedPort mode"
        );
        assert!(!result.agent.is_null(), "Agent pointer should not be null");

        // Cleanup
        let stop_result = datadog_agent_stop(result.agent);
        assert_eq!(
            stop_result,
            DatadogError::Ok,
            "Agent should stop successfully"
        );
    }
}

/// Test 2: FFI start with HttpEphemeralPort mode (operational_mode = 1)
#[test]
fn test_ffi_start_http_ephemeral_port_mode() {
    let api_key = CString::new("test-api-key-ephemeral").unwrap();
    let site = CString::new("datadoghq.com").unwrap();

    let options = DatadogAgentOptions {
        api_key: api_key.as_ptr(),
        site: site.as_ptr(),
        service: ptr::null(),
        env: ptr::null(),
        version: ptr::null(),
        appsec_enabled: 0,
        remote_config_enabled: 0,
        log_level: 1,
        operational_mode: 1, // HttpEphemeralPort
        trace_agent_port: 0, // This is ignored in ephemeral mode
        dogstatsd_enabled: 0,
        dogstatsd_port: 8125,
        trace_agent_uds_permissions: -1,
    };

    unsafe {
        let result = datadog_agent_start(&options);
        assert_eq!(
            result.error,
            DatadogError::Ok,
            "Agent should start successfully in HttpEphemeralPort mode"
        );
        assert!(!result.agent.is_null(), "Agent pointer should not be null");

        // Cleanup
        let stop_result = datadog_agent_stop(result.agent);
        assert_eq!(
            stop_result,
            DatadogError::Ok,
            "Agent should stop successfully"
        );
    }
}

/// Test 3: FFI start with invalid mode defaults to HttpFixedPort
#[test]
fn test_ffi_start_invalid_mode_defaults_to_fixed_port() {
    let api_key = CString::new("test-api-key-invalid").unwrap();
    let site = CString::new("datadoghq.com").unwrap();

    let options = DatadogAgentOptions {
        api_key: api_key.as_ptr(),
        site: site.as_ptr(),
        service: ptr::null(),
        env: ptr::null(),
        version: ptr::null(),
        appsec_enabled: 0,
        remote_config_enabled: 0,
        log_level: 1,
        operational_mode: 99,    // Invalid mode, should default to HttpFixedPort
        trace_agent_port: 18127, // Custom port to avoid conflicts
        dogstatsd_enabled: 0,
        dogstatsd_port: 8125,
        trace_agent_uds_permissions: -1,
    };

    unsafe {
        let result = datadog_agent_start(&options);
        assert_eq!(
            result.error,
            DatadogError::Ok,
            "Agent should start with default mode even with invalid operational_mode"
        );
        assert!(!result.agent.is_null(), "Agent pointer should not be null");

        // Cleanup
        let stop_result = datadog_agent_stop(result.agent);
        assert_eq!(
            stop_result,
            DatadogError::Ok,
            "Agent should stop successfully"
        );
    }
}

/// Test 4: FFI stop with null pointer
#[test]
fn test_ffi_stop_null_pointer() {
    unsafe {
        let result = datadog_agent_stop(ptr::null_mut());
        assert_eq!(
            result,
            DatadogError::NullPointer,
            "Should return NullPointer error"
        );
    }
}

/// Test 5: FFI port configuration with positive port number
#[test]
fn test_ffi_port_configuration_positive() {
    let api_key = CString::new("test-api-key-port").unwrap();
    let site = CString::new("datadoghq.com").unwrap();

    let options = DatadogAgentOptions {
        api_key: api_key.as_ptr(),
        site: site.as_ptr(),
        service: ptr::null(),
        env: ptr::null(),
        version: ptr::null(),
        appsec_enabled: 0,
        remote_config_enabled: 0,
        log_level: 1,
        operational_mode: 0,     // HttpFixedPort
        trace_agent_port: 19999, // Specific port
        dogstatsd_enabled: 0,
        dogstatsd_port: 8125,
        trace_agent_uds_permissions: -1,
    };

    unsafe {
        let result = datadog_agent_start(&options);
        assert_eq!(
            result.error,
            DatadogError::Ok,
            "Agent should start with specific port"
        );
        assert!(!result.agent.is_null(), "Agent pointer should not be null");

        // Cleanup
        let stop_result = datadog_agent_stop(result.agent);
        assert_eq!(stop_result, DatadogError::Ok);
    }
}

/// Test 6: FFI port configuration with -1 (ephemeral)
#[test]
fn test_ffi_port_configuration_ephemeral() {
    let api_key = CString::new("test-api-key-ephemeral-port").unwrap();
    let site = CString::new("datadoghq.com").unwrap();

    let options = DatadogAgentOptions {
        api_key: api_key.as_ptr(),
        site: site.as_ptr(),
        service: ptr::null(),
        env: ptr::null(),
        version: ptr::null(),
        appsec_enabled: 0,
        remote_config_enabled: 0,
        log_level: 1,
        operational_mode: 0,  // HttpFixedPort mode
        trace_agent_port: -1, // Force ephemeral port
        dogstatsd_enabled: 0,
        dogstatsd_port: 8125,
        trace_agent_uds_permissions: -1,
    };

    unsafe {
        let result = datadog_agent_start(&options);
        assert_eq!(
            result.error,
            DatadogError::Ok,
            "Agent should start with ephemeral port"
        );
        assert!(!result.agent.is_null(), "Agent pointer should not be null");

        // Cleanup
        let stop_result = datadog_agent_stop(result.agent);
        assert_eq!(stop_result, DatadogError::Ok);
    }
}

/// Test 7: FFI start with null options pointer
#[test]
fn test_ffi_start_null_options() {
    unsafe {
        let result = datadog_agent_start(ptr::null());
        assert_ne!(
            result.error,
            DatadogError::Ok,
            "Should return error for null options pointer"
        );
        assert!(result.agent.is_null(), "Agent pointer should be null");
    }
}

/// Test 8: Remote config disabled via FFI flag should not initialize store path
#[test]
fn test_ffi_remote_config_disabled_skips_initialization() {
    let api_key = CString::new("test-api-key-rc-disabled").unwrap();
    let site = CString::new("datadoghq.com").unwrap();

    let temp_dir = tempdir().expect("failed to create temp dir");
    let store_path = temp_dir.path().join("rc-store-disabled");

    let prev_store = std::env::var_os("DD_REMOTE_CONFIG_STORE_PATH");
    let prev_enabled = std::env::var_os("DD_REMOTE_CONFIGURATION_ENABLED");

    std::env::set_var("DD_REMOTE_CONFIG_STORE_PATH", &store_path);
    std::env::remove_var("DD_REMOTE_CONFIGURATION_ENABLED");

    let options = DatadogAgentOptions {
        api_key: api_key.as_ptr(),
        site: site.as_ptr(),
        service: ptr::null(),
        env: ptr::null(),
        version: ptr::null(),
        appsec_enabled: 0,
        remote_config_enabled: 0,
        log_level: 1,
        operational_mode: 0, // HttpFixedPort
        trace_agent_port: 19126,
        dogstatsd_enabled: 0,
        dogstatsd_port: 8125,
        trace_agent_uds_permissions: -1,
    };

    unsafe {
        let result = datadog_agent_start(&options);
        assert_eq!(
            result.error,
            DatadogError::Ok,
            "Agent should start successfully with remote config disabled"
        );
        assert!(!result.agent.is_null(), "Agent pointer should not be null");

        let stop_result = datadog_agent_stop(result.agent);
        assert_eq!(
            stop_result,
            DatadogError::Ok,
            "Agent should stop successfully"
        );
    }

    assert!(
        !store_path.exists(),
        "Remote config store directory should not be created when remote config is disabled"
    );

    if let Some(value) = prev_store {
        std::env::set_var("DD_REMOTE_CONFIG_STORE_PATH", value);
    } else {
        std::env::remove_var("DD_REMOTE_CONFIG_STORE_PATH");
    }

    if let Some(value) = prev_enabled {
        std::env::set_var("DD_REMOTE_CONFIGURATION_ENABLED", value);
    } else {
        std::env::remove_var("DD_REMOTE_CONFIGURATION_ENABLED");
    }
}

/// Test 8: FFI start with null API key
#[test]
fn test_ffi_start_null_api_key() {
    let site = CString::new("datadoghq.com").unwrap();

    let options = DatadogAgentOptions {
        api_key: ptr::null(), // Null API key
        site: site.as_ptr(),
        service: ptr::null(),
        env: ptr::null(),
        version: ptr::null(),
        appsec_enabled: 0,
        remote_config_enabled: 0,
        log_level: 1,
        operational_mode: 0,
        trace_agent_port: 0,
        dogstatsd_enabled: 0,
        dogstatsd_port: 8125,
        trace_agent_uds_permissions: -1,
    };

    unsafe {
        let result = datadog_agent_start(&options);
        assert_ne!(
            result.error,
            DatadogError::Ok,
            "Should return error when API key is null"
        );
        assert!(result.agent.is_null(), "Agent pointer should be null");
    }
}

/// Test 9: FFI start with empty API key
#[test]
fn test_ffi_start_empty_api_key() {
    let api_key = CString::new("").unwrap(); // Empty string
    let site = CString::new("datadoghq.com").unwrap();

    let options = DatadogAgentOptions {
        api_key: api_key.as_ptr(),
        site: site.as_ptr(),
        service: ptr::null(),
        env: ptr::null(),
        version: ptr::null(),
        appsec_enabled: 0,
        remote_config_enabled: 0,
        log_level: 1,
        operational_mode: 0,
        trace_agent_port: 0,
        dogstatsd_enabled: 0,
        dogstatsd_port: 8125,
        trace_agent_uds_permissions: -1,
    };

    unsafe {
        let result = datadog_agent_start(&options);
        assert_ne!(
            result.error,
            DatadogError::Ok,
            "Should return error when API key is empty"
        );
        assert!(result.agent.is_null(), "Agent pointer should be null");
    }
}

/// Test 10: FFI start with HttpUds mode (operational_mode = 2)
#[test]
#[cfg(unix)] // UDS is only supported on Unix platforms
fn test_ffi_start_http_uds_mode() {
    let api_key = CString::new("test-api-key-uds").unwrap();
    let site = CString::new("datadoghq.com").unwrap();

    let options = DatadogAgentOptions {
        api_key: api_key.as_ptr(),
        site: site.as_ptr(),
        service: ptr::null(),
        env: ptr::null(),
        version: ptr::null(),
        appsec_enabled: 0,
        remote_config_enabled: 0,
        log_level: 1,
        operational_mode: 2, // HttpUds
        trace_agent_port: 0, // Ignored in UDS mode
        dogstatsd_enabled: 0,
        dogstatsd_port: 8125,
        trace_agent_uds_permissions: -1,
    };

    unsafe {
        let result = datadog_agent_start(&options);
        assert_eq!(
            result.error,
            DatadogError::Ok,
            "Agent should start successfully in HttpUds mode"
        );
        assert!(!result.agent.is_null(), "Agent pointer should not be null");
        assert_eq!(
            result.bound_port, -1,
            "bound_port should be -1 in HttpUds mode"
        );
        assert!(
            !result.uds_path.is_null(),
            "UDS path should not be null in HttpUds mode"
        );

        // Validate UDS path format
        let uds_path = std::ffi::CStr::from_ptr(result.uds_path).to_str().unwrap();
        assert!(
            uds_path.starts_with("/tmp/dd-trace-"),
            "UDS path should start with /tmp/dd-trace-"
        );
        assert!(
            uds_path.ends_with(".sock"),
            "UDS path should end with .sock"
        );

        // Free the UDS path string
        datadog_free_string(result.uds_path as *mut std::os::raw::c_char);

        // Cleanup
        let stop_result = datadog_agent_stop(result.agent);
        assert_eq!(
            stop_result,
            DatadogError::Ok,
            "Agent should stop successfully"
        );
    }
}

/// Test 11: FFI HttpUds mode - verify UDS path contains PID
#[test]
#[cfg(unix)]
fn test_ffi_uds_path_contains_pid() {
    let api_key = CString::new("test-api-key-uds-pid").unwrap();
    let site = CString::new("datadoghq.com").unwrap();

    let options = DatadogAgentOptions {
        api_key: api_key.as_ptr(),
        site: site.as_ptr(),
        service: ptr::null(),
        env: ptr::null(),
        version: ptr::null(),
        appsec_enabled: 0,
        remote_config_enabled: 0,
        log_level: 1,
        operational_mode: 2, // HttpUds
        trace_agent_port: 0,
        dogstatsd_enabled: 0,
        dogstatsd_port: 8125,
        trace_agent_uds_permissions: -1,
    };

    unsafe {
        let result = datadog_agent_start(&options);
        assert_eq!(
            result.error,
            DatadogError::Ok,
            "Agent should start successfully"
        );
        assert!(!result.agent.is_null(), "Agent pointer should not be null");
        assert!(!result.uds_path.is_null(), "UDS path should not be null");

        // Check that UDS path contains the process ID
        let uds_path = std::ffi::CStr::from_ptr(result.uds_path).to_str().unwrap();
        let pid = std::process::id();
        assert!(
            uds_path.contains(&pid.to_string()),
            "UDS path should contain process ID"
        );

        // Free the UDS path string
        datadog_free_string(result.uds_path as *mut std::os::raw::c_char);

        // Cleanup
        datadog_agent_stop(result.agent);
    }
}

/// Test 12: FFI HttpUds mode - verify socket file is created
#[test]
#[cfg(unix)]
fn test_ffi_uds_socket_file_created() {
    use std::path::Path;

    let api_key = CString::new("test-api-key-uds-file").unwrap();
    let site = CString::new("datadoghq.com").unwrap();

    let options = DatadogAgentOptions {
        api_key: api_key.as_ptr(),
        site: site.as_ptr(),
        service: ptr::null(),
        env: ptr::null(),
        version: ptr::null(),
        appsec_enabled: 0,
        remote_config_enabled: 0,
        log_level: 1,
        operational_mode: 2, // HttpUds
        trace_agent_port: 0,
        dogstatsd_enabled: 0,
        dogstatsd_port: 8125,
        trace_agent_uds_permissions: -1,
    };

    unsafe {
        let result = datadog_agent_start(&options);
        assert_eq!(
            result.error,
            DatadogError::Ok,
            "Agent should start successfully"
        );
        assert!(!result.agent.is_null(), "Agent pointer should not be null");
        assert!(!result.uds_path.is_null(), "UDS path should not be null");

        // Get the UDS path
        let uds_path_cstr = std::ffi::CStr::from_ptr(result.uds_path).to_str().unwrap();
        let uds_path = uds_path_cstr.to_string();

        // Give the agent a moment to bind the socket
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Verify socket file exists while agent is running
        assert!(
            Path::new(&uds_path).exists(),
            "Socket file should exist at {}",
            uds_path
        );

        // Free the UDS path string
        datadog_free_string(result.uds_path as *mut std::os::raw::c_char);

        // Cleanup
        datadog_agent_stop(result.agent);

        // Give cleanup time to complete
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Verify socket file is cleaned up after agent stops
        // (It should be automatically removed by UnixSocketCleanupGuard)
        assert!(
            !Path::new(&uds_path).exists(),
            "Socket file should be cleaned up after agent stops at {}",
            uds_path
        );
    }
}

/// Test 13: FFI start with HttpUds mode with custom permissions
/// Tests that the trace_agent_uds_permissions FFI field is properly wired up
#[test]
#[cfg(unix)]
fn test_ffi_uds_mode_custom_permissions() {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    let api_key = CString::new("test-api-key-uds-perms").unwrap();
    let site = CString::new("datadoghq.com").unwrap();

    // Test with custom permissions: 0o660 (432 in decimal) = owner + group read/write
    let options = DatadogAgentOptions {
        api_key: api_key.as_ptr(),
        site: site.as_ptr(),
        service: ptr::null(),
        env: ptr::null(),
        version: ptr::null(),
        appsec_enabled: 0,
        remote_config_enabled: 0,
        log_level: 1,
        operational_mode: 2, // HttpUds
        trace_agent_port: 0,
        dogstatsd_enabled: 0,
        dogstatsd_port: 8125,
        trace_agent_uds_permissions: 0o660, // Custom permissions: owner + group
    };

    unsafe {
        let result = datadog_agent_start(&options);
        assert_eq!(
            result.error,
            DatadogError::Ok,
            "Agent should start successfully with custom UDS permissions"
        );
        assert!(!result.agent.is_null(), "Agent pointer should not be null");
        assert!(!result.uds_path.is_null(), "UDS path should not be null");

        // Get the UDS path
        let uds_path_cstr = std::ffi::CStr::from_ptr(result.uds_path).to_str().unwrap();
        let uds_path = uds_path_cstr.to_string();

        // Verify socket file exists
        assert!(
            Path::new(&uds_path).exists(),
            "Socket file should exist at {}",
            uds_path
        );

        // Check actual file permissions
        let metadata =
            std::fs::metadata(&uds_path).expect("Should be able to read socket file metadata");
        let permissions = metadata.permissions();
        let mode = permissions.mode();

        // Extract permission bits (mask out file type bits)
        let perm_bits = mode & 0o777;

        assert_eq!(
            perm_bits, 0o660,
            "Socket file should have 0o660 permissions, but has 0o{:o}",
            perm_bits
        );

        // Free the UDS path string
        datadog_free_string(result.uds_path as *mut std::os::raw::c_char);

        // Cleanup
        datadog_agent_stop(result.agent);

        // Give cleanup time to complete
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Verify socket file is cleaned up after agent stops
        assert!(
            !Path::new(&uds_path).exists(),
            "Socket file should be cleaned up after agent stops at {}",
            uds_path
        );
    }
}

/// Test 14: FFI start with HttpUds mode with default permissions (-1)
/// Tests that trace_agent_uds_permissions = -1 uses the secure default (0o600)
#[test]
#[cfg(unix)]
fn test_ffi_uds_mode_default_permissions() {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    let api_key = CString::new("test-api-key-uds-default").unwrap();
    let site = CString::new("datadoghq.com").unwrap();

    // Test with -1 (should use default 0o600)
    let options = DatadogAgentOptions {
        api_key: api_key.as_ptr(),
        site: site.as_ptr(),
        service: ptr::null(),
        env: ptr::null(),
        version: ptr::null(),
        appsec_enabled: 0,
        remote_config_enabled: 0,
        log_level: 1,
        operational_mode: 2, // HttpUds
        trace_agent_port: 0,
        dogstatsd_enabled: 0,
        dogstatsd_port: 8125,
        trace_agent_uds_permissions: -1, // Use default (should be 0o600)
    };

    unsafe {
        let result = datadog_agent_start(&options);
        assert_eq!(
            result.error,
            DatadogError::Ok,
            "Agent should start successfully with default UDS permissions"
        );
        assert!(!result.agent.is_null(), "Agent pointer should not be null");
        assert!(!result.uds_path.is_null(), "UDS path should not be null");

        // Get the UDS path
        let uds_path_cstr = std::ffi::CStr::from_ptr(result.uds_path).to_str().unwrap();
        let uds_path = uds_path_cstr.to_string();

        // Verify socket file exists
        assert!(
            Path::new(&uds_path).exists(),
            "Socket file should exist at {}",
            uds_path
        );

        // Check actual file permissions - should be default 0o600
        let metadata =
            std::fs::metadata(&uds_path).expect("Should be able to read socket file metadata");
        let permissions = metadata.permissions();
        let mode = permissions.mode();

        // Extract permission bits (mask out file type bits)
        let perm_bits = mode & 0o777;

        assert_eq!(
            perm_bits, 0o600,
            "Socket file should have default 0o600 permissions (owner-only), but has 0o{:o}",
            perm_bits
        );

        // Free the UDS path string
        datadog_free_string(result.uds_path as *mut std::os::raw::c_char);

        // Cleanup
        datadog_agent_stop(result.agent);

        // Give cleanup time to complete
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Verify socket file is cleaned up after agent stops
        assert!(
            !Path::new(&uds_path).exists(),
            "Socket file should be cleaned up after agent stops at {}",
            uds_path
        );
    }
}
