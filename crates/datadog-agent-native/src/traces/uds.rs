//! Unix Domain Socket (UDS) utilities for the trace agent.
//!
//! This module provides functionality for generating Unix Domain Socket paths
//! and managing UDS-based communication for the trace agent.

use std::io;
use std::net::SocketAddr;
use std::path::Path;
use tokio::net::{TcpListener, UnixListener};

use crate::config::{operational_mode::OperationalMode, Config};

/// Validates a Unix Domain Socket path.
///
/// # Errors
///
/// Returns an error if:
/// - Path is empty
/// - Path length exceeds 100 bytes (safe margin under Unix socket limit of 108)
/// - Path is not absolute (doesn't start with `/` on Unix)
/// - Path contains null bytes
///
/// # Examples
///
/// ```rust,no_run
/// # use datadog_agent_native::traces::uds::validate_uds_path;
/// # use std::io;
/// # fn example() -> io::Result<()> {
/// validate_uds_path("/tmp/my-socket.sock")?; // OK
/// validate_uds_path("")?; // Error: empty path
/// # Ok(())
/// # }
/// ```
#[cfg(unix)]
pub fn validate_uds_path(path: &str) -> io::Result<()> {
    // Check if path is empty
    if path.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Unix socket path cannot be empty",
        ));
    }

    // Check for null bytes which would cause issues with C APIs
    if path.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Unix socket path cannot contain null bytes",
        ));
    }

    // Check path byte length (Unix sockets have a limit of 108 bytes on Linux, use 100 for safety)
    // Use byte length, not character length, as the limit is in bytes
    let byte_len = path.as_bytes().len();
    if byte_len > 100 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "Unix socket path too long: {} bytes (maximum 100 bytes). Path: {}",
                byte_len, path
            ),
        ));
    }

    // Check if path is absolute (best practice, not strictly required)
    if !path.starts_with('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "Unix socket path should be absolute (start with '/'). Got: {}",
                path
            ),
        ));
    }

    Ok(())
}

/// Generates a Unix Domain Socket path based on the current process ID and a random suffix.
///
/// The generated path ensures uniqueness across multiple agent instances by
/// incorporating both the process ID and a random component. This prevents
/// socket path collisions even when PIDs are reused quickly after process restarts.
///
/// # Platform-specific behavior
///
/// - **Unix/Linux/macOS**: Returns `/tmp/dd-trace-{PID}-{RANDOM}.sock`
/// - **Windows**: Returns `\\.\\pipe\\dd-trace-{PID}-{RANDOM}` (named pipe)
///
/// Where `{RANDOM}` is a 6-digit hexadecimal value (e.g., `a3f2b1`)
///
/// # Examples
///
/// ```rust,no_run
/// # use datadog_agent_native::traces::uds::generate_uds_path;
/// let path = generate_uds_path().expect("Failed to generate UDS path");
/// println!("UDS path: {}", path);
/// // Unix: /tmp/dd-trace-12345-a3f2b1.sock
/// // Windows: \\.\\pipe\\dd-trace-12345-a3f2b1
/// ```
///
/// # Errors
///
/// Returns an error if:
/// - The process ID cannot be obtained (should never happen in normal circumstances)
/// - The generated path would exceed system limits (108 chars on Linux for Unix sockets)
pub fn generate_uds_path() -> io::Result<String> {
    let pid = std::process::id();
    // Add random component to avoid PID reuse collisions
    // Use 6 hex digits (3 bytes) for good uniqueness without excessive length
    let random_suffix = fastrand::u32(..0x1000000); // 0 to 16,777,215 (6 hex digits)

    #[cfg(unix)]
    {
        let path = format!("/tmp/dd-trace-{}-{:06x}.sock", pid, random_suffix);

        // Unix socket paths have a platform-specific length limit (typically 108 on Linux)
        // Check if our path would exceed reasonable limits
        // Use byte length, not character length, for consistency with validate_uds_path()
        let byte_len = path.as_bytes().len();
        if byte_len > 100 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Generated UDS path too long: {} bytes", byte_len),
            ));
        }

        Ok(path)
    }

    #[cfg(windows)]
    {
        // Windows named pipes don't have the same length restrictions as Unix sockets
        Ok(format!(r"\\.\pipe\dd-trace-{}-{:06x}", pid, random_suffix))
    }

    #[cfg(not(any(unix, windows)))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Unix Domain Sockets are only supported on Unix and Windows platforms",
        ))
    }
}

/// Guard that cleans up a Unix socket file when dropped.
///
/// Tokio's UnixListener does NOT automatically remove the socket file when dropped,
/// which can lead to socket file accumulation in `/tmp`. This guard ensures
/// proper cleanup by storing the socket path and removing the file in its Drop implementation.
///
/// The guard should be kept alive as long as the socket is in use, and will automatically
/// clean up the socket file when dropped.
#[cfg(unix)]
#[derive(Debug)]
pub struct UnixSocketCleanupGuard {
    path: String,
}

#[cfg(unix)]
impl UnixSocketCleanupGuard {
    /// Creates a new cleanup guard for the given socket path.
    ///
    /// # Arguments
    ///
    /// * `path` - The filesystem path of the socket file (will be removed on drop)
    pub fn new(path: String) -> Self {
        Self { path }
    }
}

#[cfg(unix)]
impl Drop for UnixSocketCleanupGuard {
    fn drop(&mut self) {
        // Best effort cleanup - don't panic if removal fails
        // The file might already be removed or permissions might prevent deletion
        if let Err(e) = std::fs::remove_file(&self.path) {
            // Only log if the error is not "file not found" (already cleaned up)
            if e.kind() != io::ErrorKind::NotFound {
                eprintln!(
                    "Warning: Failed to remove Unix socket file '{}': {}",
                    self.path, e
                );
            }
        }
    }
}

/// Listener abstraction that supports both TCP and Unix Domain Sockets.
///
/// This enum allows the trace agent to accept connections via either:
/// - TCP sockets (fixed or ephemeral ports)
/// - Unix Domain Sockets (filesystem-based)
///
/// Note: For Unix sockets, a `UnixSocketCleanupGuard` should be kept alive
/// to ensure the socket file is removed when the server stops.
pub enum Listener {
    /// TCP listener bound to a socket address
    Tcp(TcpListener),
    /// Unix Domain Socket listener bound to a filesystem path
    #[cfg(unix)]
    Unix(UnixListener),
}

/// Information about the created listener.
///
/// Contains details about where the listener is bound, suitable for
/// returning to the user via FFI or logging.
///
/// For Unix sockets, this struct also holds a cleanup guard wrapped in an Arc.
/// The socket file is automatically removed when the LAST reference is dropped.
#[derive(Debug, Clone)]
pub struct ListenerInfo {
    /// TCP port if using TCP listener (Some for TCP, None for UDS)
    pub tcp_port: Option<u16>,
    /// Unix socket path if using UDS (Some for UDS, None for TCP)
    pub uds_path: Option<String>,
    /// Cleanup guard for Unix sockets (automatically removes socket file on drop)
    /// Wrapped in Arc so cleanup only happens when the last reference is dropped
    #[cfg(unix)]
    _cleanup_guard: Option<std::sync::Arc<UnixSocketCleanupGuard>>,
}

impl ListenerInfo {
    /// Creates a new ListenerInfo for TCP listeners.
    ///
    /// This constructor is useful for tests and other scenarios where you need to create
    /// a ListenerInfo without going through `create_listener()`.
    ///
    /// # Arguments
    ///
    /// * `tcp_port` - Optional TCP port number (Some for TCP, None for UDS)
    /// * `uds_path` - Optional Unix socket path (Some for UDS, None for TCP)
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use datadog_agent_native::traces::uds::ListenerInfo;
    /// // Create TCP listener info
    /// let tcp_info = ListenerInfo::new(Some(8126), None);
    ///
    /// // Create UDS listener info (no cleanup guard - for testing only)
    /// let uds_info = ListenerInfo::new(None, Some("/tmp/test.sock".to_string()));
    /// ```
    pub fn new(tcp_port: Option<u16>, uds_path: Option<String>) -> Self {
        Self {
            tcp_port,
            uds_path,
            #[cfg(unix)]
            _cleanup_guard: None,
        }
    }
}

/// Creates a listener based on the operational mode in the configuration.
///
/// This function handles all three HTTP operational modes:
/// - `HttpFixedPort`: Creates TCP listener on configured port (or default 8126)
/// - `HttpEphemeralPort`: Creates TCP listener on OS-assigned port (0)
/// - `HttpUds`: Creates Unix Domain Socket listener
///
/// # Arguments
///
/// * `config` - Agent configuration containing operational mode and settings
///
/// # Returns
///
/// Returns a tuple of (Listener, ListenerInfo) containing:
/// - The created listener (TCP or Unix)
/// - Information about where it's bound (port or path)
///
/// # Errors
///
/// Returns an error if:
/// - TCP socket fails to bind (port in use, permission denied, etc.)
/// - Unix socket path generation fails
/// - Unix socket file already exists and can't be removed
/// - Unix socket fails to bind or set permissions
///
/// # Examples
///
/// ```rust,no_run
/// # use datadog_agent_native::config::Config;
/// # use datadog_agent_native::traces::uds::create_listener;
/// # async fn example() -> std::io::Result<()> {
/// let config = Config::default();
/// let (listener, info) = create_listener(&config).await?;
///
/// if let Some(port) = info.tcp_port {
///     println!("Listening on TCP port {}", port);
/// }
/// if let Some(path) = info.uds_path {
///     println!("Listening on Unix socket {}", path);
/// }
/// # Ok(())
/// # }
/// ```
pub async fn create_listener(config: &Config) -> io::Result<(Listener, ListenerInfo)> {
    match config.operational_mode {
        OperationalMode::HttpFixedPort | OperationalMode::HttpEphemeralPort => {
            // Determine port based on operational mode
            let port = if config.operational_mode.uses_ephemeral_ports() {
                0 // Let OS assign
            } else {
                config.trace_agent_port.unwrap_or(8126)
            };

            // Bind TCP listener
            let socket = SocketAddr::from(([127, 0, 0, 1], port));
            let listener = TcpListener::bind(&socket).await.map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!(
                        "Failed to bind Trace Agent to TCP port {} (mode: {}). Port may already be in use. Error: {}",
                        port,
                        config.operational_mode,
                        e
                    )
                )
            })?;

            // Get actual bound port
            let actual_port = listener.local_addr()?.port();

            Ok((
                Listener::Tcp(listener),
                ListenerInfo {
                    tcp_port: Some(actual_port),
                    uds_path: None,
                    #[cfg(unix)]
                    _cleanup_guard: None,
                },
            ))
        }

        #[cfg(unix)]
        OperationalMode::HttpUds => {
            // Determine UDS path (configured or auto-generated)
            let uds_path = if let Some(ref path) = config.trace_agent_uds_path {
                // Validate custom path
                validate_uds_path(path)?;

                // Verify parent directory exists
                if let Some(parent) = Path::new(path).parent() {
                    if !parent.exists() {
                        return Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            format!(
                                "Parent directory does not exist for Unix socket path: {}",
                                path
                            ),
                        ));
                    }
                }

                path.clone()
            } else {
                generate_uds_path()?
            };

            let socket_path = Path::new(&uds_path);

            // Remove existing socket file if it exists
            // This handles cases where the agent didn't clean up properly on last exit
            // SECURITY: Check that it's not a symlink to prevent symlink attacks
            if socket_path.exists() {
                // Use symlink_metadata to not follow symlinks
                let metadata = std::fs::symlink_metadata(&uds_path).map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!(
                            "Failed to check metadata of existing file at {}: {}",
                            uds_path, e
                        ),
                    )
                })?;

                // Reject symlinks for security
                if metadata.file_type().is_symlink() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "Unix socket path is a symlink, refusing to remove for security: {}",
                            uds_path
                        ),
                    ));
                }

                // Safe to remove - it's a regular file/socket, not a symlink
                std::fs::remove_file(&uds_path).map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!(
                            "Failed to remove existing Unix socket at {}: {}. \
                             Socket may be in use by another process.",
                            uds_path, e
                        ),
                    )
                })?;
            }

            // Bind Unix listener
            let listener = UnixListener::bind(&uds_path).map_err(|e| {
                let msg = match e.kind() {
                    io::ErrorKind::AddrInUse | io::ErrorKind::AlreadyExists => {
                        format!(
                            "Failed to bind Trace Agent to Unix socket {} (mode: {}). \
                             Socket path already in use or bound by another process. Error: {}",
                            uds_path, config.operational_mode, e
                        )
                    }
                    io::ErrorKind::PermissionDenied => {
                        format!(
                            "Failed to bind Trace Agent to Unix socket {} (mode: {}). \
                             Permission denied - check directory permissions. Error: {}",
                            uds_path, config.operational_mode, e
                        )
                    }
                    _ => {
                        format!(
                            "Failed to bind Trace Agent to Unix socket {} (mode: {}). Error: {}",
                            uds_path, config.operational_mode, e
                        )
                    }
                };
                io::Error::new(e.kind(), msg)
            })?;

            // Set permissions on the socket file
            #[cfg(unix)]
            {
                use std::fs;
                use std::os::unix::fs::PermissionsExt;

                let permissions = fs::Permissions::from_mode(config.trace_agent_uds_permissions);
                fs::set_permissions(&uds_path, permissions).map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!(
                            "Failed to set permissions {:o} on Unix socket {}: {}",
                            config.trace_agent_uds_permissions, uds_path, e
                        ),
                    )
                })?;
            }

            // Create cleanup guard wrapped in Arc to ensure socket file is removed
            // when the LAST ListenerInfo reference is dropped
            let cleanup_guard = std::sync::Arc::new(UnixSocketCleanupGuard::new(uds_path.clone()));

            Ok((
                Listener::Unix(listener),
                ListenerInfo {
                    tcp_port: None,
                    uds_path: Some(uds_path),
                    _cleanup_guard: Some(cleanup_guard),
                },
            ))
        }

        #[cfg(not(unix))]
        OperationalMode::HttpUds => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Unix Domain Sockets are only supported on Unix platforms",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_uds_path_contains_pid() {
        let path = generate_uds_path().expect("Failed to generate UDS path");
        let pid = std::process::id();
        let pid_str = pid.to_string();

        assert!(
            path.contains(&pid_str),
            "Path '{}' should contain PID '{}'",
            path,
            pid_str
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_generate_uds_path_unix_format() {
        let path = generate_uds_path().expect("Failed to generate UDS path");
        let pid = std::process::id();

        assert!(
            path.starts_with("/tmp/dd-trace-"),
            "Path should start with /tmp/dd-trace-"
        );
        assert!(path.ends_with(".sock"), "Path should end with .sock");

        // Path format: /tmp/dd-trace-{PID}-{RANDOM}.sock where RANDOM is 6 hex digits
        // Example: /tmp/dd-trace-12345-a3f2b1.sock
        let expected_prefix = format!("/tmp/dd-trace-{}-", pid);
        assert!(
            path.starts_with(&expected_prefix),
            "Path should start with {}",
            expected_prefix
        );

        // Extract the random suffix part (between last '-' and '.sock')
        let random_part = path
            .strip_prefix(&expected_prefix)
            .and_then(|s| s.strip_suffix(".sock"))
            .expect("Path should match expected format");

        assert_eq!(random_part.len(), 6, "Random suffix should be 6 hex digits");
        assert!(
            random_part.chars().all(|c| c.is_ascii_hexdigit()),
            "Random suffix should contain only hex digits, got: {}",
            random_part
        );
    }

    #[test]
    #[cfg(windows)]
    fn test_generate_uds_path_windows_format() {
        let path = generate_uds_path().expect("Failed to generate UDS path");
        let pid = std::process::id();

        assert!(
            path.starts_with(r"\\.\pipe\dd-trace-"),
            "Path should start with \\\\.\\pipe\\dd-trace-"
        );

        // Path format: \\.\pipe\dd-trace-{PID}-{RANDOM} where RANDOM is 6 hex digits
        // Example: \\.\pipe\dd-trace-12345-a3f2b1
        let expected_prefix = format!(r"\\.\pipe\dd-trace-{}-", pid);
        assert!(
            path.starts_with(&expected_prefix),
            "Path should start with {}",
            expected_prefix
        );

        // Extract the random suffix part (after last '-')
        let random_part = path
            .strip_prefix(&expected_prefix)
            .expect("Path should match expected format");

        assert_eq!(random_part.len(), 6, "Random suffix should be 6 hex digits");
        assert!(
            random_part.chars().all(|c| c.is_ascii_hexdigit()),
            "Random suffix should contain only hex digits, got: {}",
            random_part
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_generate_uds_path_length_reasonable() {
        let path = generate_uds_path().expect("Failed to generate UDS path");

        // Unix socket paths typically have a limit of 108 bytes on Linux
        // Our paths should be well under this limit
        assert!(
            path.len() < 100,
            "Path length {} should be less than 100 bytes",
            path.len()
        );
    }

    #[test]
    fn test_generate_uds_path_uniqueness() {
        // Generate path multiple times in the same process - should be DIFFERENT due to random suffix
        // This prevents collisions even if PIDs are reused quickly after process restarts
        let path1 = generate_uds_path().expect("Failed to generate UDS path");
        let path2 = generate_uds_path().expect("Failed to generate UDS path");

        assert_ne!(
            path1, path2,
            "Paths should be unique within same process due to random suffix"
        );

        // However, both paths should have the same PID prefix
        let pid = std::process::id();
        #[cfg(unix)]
        {
            let expected_prefix = format!("/tmp/dd-trace-{}-", pid);
            assert!(
                path1.starts_with(&expected_prefix),
                "Path1 should contain PID"
            );
            assert!(
                path2.starts_with(&expected_prefix),
                "Path2 should contain PID"
            );
        }
        #[cfg(windows)]
        {
            let expected_prefix = format!(r"\\.\pipe\dd-trace-{}-", pid);
            assert!(
                path1.starts_with(&expected_prefix),
                "Path1 should contain PID"
            );
            assert!(
                path2.starts_with(&expected_prefix),
                "Path2 should contain PID"
            );
        }
    }

    #[test]
    fn test_generate_uds_path_no_spaces() {
        let path = generate_uds_path().expect("Failed to generate UDS path");

        assert!(
            !path.contains(' '),
            "Path should not contain spaces: '{}'",
            path
        );
    }

    #[test]
    fn test_generate_uds_path_alphanumeric() {
        let _path = generate_uds_path().expect("Failed to generate UDS path");
        let pid = std::process::id();
        let pid_str = pid.to_string();

        // Extract just the PID portion from the path
        assert!(
            pid_str.chars().all(|c| c.is_ascii_digit()),
            "PID should only contain digits"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_validate_uds_path_valid() {
        // Valid absolute path should pass
        assert!(validate_uds_path("/tmp/test.sock").is_ok());
        assert!(validate_uds_path("/var/run/dd-trace.sock").is_ok());
        assert!(validate_uds_path("/tmp/dd-trace-12345.sock").is_ok());
    }

    #[test]
    #[cfg(unix)]
    fn test_validate_uds_path_empty() {
        // Empty path should fail
        let result = validate_uds_path("");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("cannot be empty"));
    }

    #[test]
    #[cfg(unix)]
    fn test_validate_uds_path_too_long() {
        // Path exceeding 100 bytes should fail
        let long_path = format!("/tmp/{}.sock", "a".repeat(100));
        let result = validate_uds_path(&long_path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("too long"));
        assert!(err.to_string().contains("maximum 100 bytes"));
    }

    #[test]
    #[cfg(unix)]
    fn test_validate_uds_path_relative() {
        // Relative paths should fail
        let result = validate_uds_path("relative/path.sock");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("absolute"));
        assert!(err.to_string().contains("start with '/'"));
    }

    #[test]
    #[cfg(unix)]
    fn test_validate_uds_path_edge_cases() {
        // Exactly 100 bytes should pass
        let path_100 = format!("/{}", "a".repeat(99));
        assert_eq!(path_100.len(), 100);
        assert!(validate_uds_path(&path_100).is_ok());

        // 101 bytes should fail
        let path_101 = format!("/{}", "a".repeat(100));
        assert_eq!(path_101.len(), 101);
        assert!(validate_uds_path(&path_101).is_err());

        // Just "/" should pass (though not practical)
        assert!(validate_uds_path("/").is_ok());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_create_listener_custom_uds_path() {
        use crate::config::{operational_mode::OperationalMode, Config};
        use tempfile::TempDir;

        // Create a temporary directory for our socket
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let custom_path = temp_dir.path().join("custom-trace.sock");
        let custom_path_str = custom_path.to_str().expect("Invalid path").to_string();

        // Create config with custom UDS path
        let mut config = Config::default();
        config.operational_mode = OperationalMode::HttpUds;
        config.trace_agent_uds_path = Some(custom_path_str.clone());

        // Create listener
        let (listener, info) = create_listener(&config)
            .await
            .expect("Failed to create listener");

        // Verify the custom path was used
        assert!(info.uds_path.is_some());
        assert_eq!(info.uds_path.as_ref().unwrap(), &custom_path_str);
        assert!(info.tcp_port.is_none());

        // Verify socket file exists
        assert!(custom_path.exists(), "Socket file should exist");

        // Clean up
        drop(listener);
        drop(info); // This should trigger cleanup via the Arc guard
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_create_listener_custom_uds_permissions() {
        use crate::config::{operational_mode::OperationalMode, Config};
        use std::os::unix::fs::PermissionsExt;
        use tempfile::TempDir;

        // Create a temporary directory for our socket
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let custom_path = temp_dir.path().join("perm-test.sock");
        let custom_path_str = custom_path.to_str().expect("Invalid path").to_string();

        // Create config with custom permissions (0o600 = owner read/write only)
        let mut config = Config::default();
        config.operational_mode = OperationalMode::HttpUds;
        config.trace_agent_uds_path = Some(custom_path_str.clone());
        config.trace_agent_uds_permissions = 0o600;

        // Create listener
        let (listener, info) = create_listener(&config)
            .await
            .expect("Failed to create listener");

        // Verify socket file exists and has correct permissions
        assert!(custom_path.exists(), "Socket file should exist");

        let metadata = std::fs::metadata(&custom_path).expect("Failed to get metadata");
        let permissions = metadata.permissions();
        let mode = permissions.mode();

        // Extract permission bits (last 9 bits)
        let perm_bits = mode & 0o777;
        assert_eq!(perm_bits, 0o600, "Socket should have 0o600 permissions");

        // Clean up
        drop(listener);
        drop(info);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_create_listener_invalid_custom_path() {
        use crate::config::{operational_mode::OperationalMode, Config};

        // Test empty path
        let mut config = Config::default();
        config.operational_mode = OperationalMode::HttpUds;
        config.trace_agent_uds_path = Some("".to_string());

        let result = create_listener(&config).await;
        assert!(result.is_err(), "Empty path should fail validation");
        if let Err(e) = result {
            assert!(e.to_string().contains("cannot be empty"));
        }

        // Test relative path
        config.trace_agent_uds_path = Some("relative/path.sock".to_string());
        let result = create_listener(&config).await;
        assert!(result.is_err(), "Relative path should fail validation");
        if let Err(e) = result {
            assert!(e.to_string().contains("absolute"));
        }

        // Test too-long path
        let long_path = format!("/tmp/{}.sock", "a".repeat(100));
        config.trace_agent_uds_path = Some(long_path);
        let result = create_listener(&config).await;
        assert!(result.is_err(), "Too-long path should fail validation");
        if let Err(e) = result {
            assert!(e.to_string().contains("too long"));
        }
    }

    #[test]
    #[cfg(unix)]
    fn test_validate_uds_path_null_byte() {
        // Path with embedded null byte should fail
        let path_with_null = "/tmp/test\0.sock";
        let result = validate_uds_path(path_with_null);
        assert!(result.is_err(), "Path with null byte should fail");

        if let Err(e) = result {
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
            assert!(
                e.to_string().contains("null bytes"),
                "Error should mention null bytes, got: {}",
                e
            );
        }

        // Path with null byte in the middle
        let path_with_null = "/tmp/te\0st.sock";
        let result = validate_uds_path(path_with_null);
        assert!(result.is_err(), "Path with null byte in middle should fail");
    }

    #[test]
    #[cfg(unix)]
    fn test_validate_uds_path_unicode() {
        // Test that byte length is correctly calculated for Unicode characters
        // Unicode characters can be 2-3 bytes each in UTF-8

        // Path with Unicode characters that is short in character count but long in bytes
        // Each emoji is typically 4 bytes in UTF-8
        // 🔥 (fire emoji) = 4 bytes, so we need 25+ to exceed 100 bytes
        // "/tmp/.sock" = 10 bytes, so we need (100 - 10) / 4 = 23+ emojis
        let emoji_path = format!("/tmp/{}.sock", "🔥".repeat(25));
        let byte_len = emoji_path.as_bytes().len();

        // This should be > 100 bytes due to multi-byte characters
        // 25 emojis * 4 bytes + 10 bytes for "/tmp/.sock" = 110 bytes
        assert!(
            byte_len > 100,
            "Emoji path should exceed 100 bytes, got {} bytes",
            byte_len
        );

        let result = validate_uds_path(&emoji_path);
        assert!(result.is_err(), "Path exceeding 100 bytes should fail");

        if let Err(e) = result {
            assert!(
                e.to_string().contains("too long"),
                "Error should mention path too long, got: {}",
                e
            );
            assert!(
                e.to_string().contains(&byte_len.to_string()),
                "Error should include actual byte length"
            );
        }

        // Test a Unicode path that's within limits
        let short_unicode = "/tmp/test-🔥.sock";
        let result = validate_uds_path(short_unicode);
        assert!(result.is_ok(), "Short Unicode path should be valid");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_create_listener_rejects_symlink() {
        use crate::config::{operational_mode::OperationalMode, Config};
        use std::os::unix::fs as unix_fs;
        use tempfile::TempDir;

        // Create a temporary directory
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let socket_path = temp_dir.path().join("test.sock");
        let target_path = temp_dir.path().join("target-file");

        // Create a target file
        std::fs::write(&target_path, b"important data").expect("Failed to create target file");

        // Create a symlink at the socket path pointing to the target
        unix_fs::symlink(&target_path, &socket_path).expect("Failed to create symlink");

        let socket_path_str = socket_path.to_str().expect("Invalid path").to_string();

        // Try to create listener - should reject the symlink
        let mut config = Config::default();
        config.operational_mode = OperationalMode::HttpUds;
        config.trace_agent_uds_path = Some(socket_path_str);

        let result = create_listener(&config).await;
        assert!(result.is_err(), "Should reject symlink for security");

        if let Err(e) = result {
            assert!(
                e.to_string().contains("symlink"),
                "Error should mention symlink, got: {}",
                e
            );
            assert!(
                e.to_string().contains("refusing to remove"),
                "Error should explain we're refusing to remove it"
            );
        }

        // Verify the target file was NOT deleted (security check passed)
        assert!(
            target_path.exists(),
            "Target file should still exist - symlink attack prevented"
        );
        let content = std::fs::read_to_string(&target_path).expect("Failed to read target");
        assert_eq!(
            content, "important data",
            "Target file content should be unchanged"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_create_listener_missing_parent_directory() {
        use crate::config::{operational_mode::OperationalMode, Config};

        // Use a path in a non-existent directory
        let non_existent_path = "/tmp/this-dir-definitely-does-not-exist-12345/test.sock";

        let mut config = Config::default();
        config.operational_mode = OperationalMode::HttpUds;
        config.trace_agent_uds_path = Some(non_existent_path.to_string());

        let result = create_listener(&config).await;
        assert!(
            result.is_err(),
            "Should fail when parent directory doesn't exist"
        );

        if let Err(e) = result {
            assert_eq!(e.kind(), io::ErrorKind::NotFound);
            assert!(
                e.to_string().contains("Parent directory does not exist"),
                "Error should explain parent directory issue, got: {}",
                e
            );
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_create_listener_null_byte_in_path() {
        use crate::config::{operational_mode::OperationalMode, Config};

        // Path with null byte should be rejected during validation
        let path_with_null = "/tmp/test\0.sock".to_string();

        let mut config = Config::default();
        config.operational_mode = OperationalMode::HttpUds;
        config.trace_agent_uds_path = Some(path_with_null);

        let result = create_listener(&config).await;
        assert!(result.is_err(), "Should reject path with null byte");

        if let Err(e) = result {
            assert!(
                e.to_string().contains("null bytes"),
                "Error should mention null bytes, got: {}",
                e
            );
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_create_listener_removes_stale_socket() {
        use crate::config::{operational_mode::OperationalMode, Config};
        use tempfile::TempDir;

        // Create a temporary directory
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let socket_path = temp_dir.path().join("stale.sock");
        let socket_path_str = socket_path.to_str().expect("Invalid path").to_string();

        // Create a stale socket file (simulate previous unclean shutdown)
        std::fs::write(&socket_path, b"stale socket").expect("Failed to create stale file");
        assert!(socket_path.exists(), "Stale socket should exist");

        // Create listener - should remove stale socket and create new one
        let mut config = Config::default();
        config.operational_mode = OperationalMode::HttpUds;
        config.trace_agent_uds_path = Some(socket_path_str);

        let result = create_listener(&config).await;
        assert!(
            result.is_ok(),
            "Should successfully remove stale socket and create new listener"
        );

        // Verify socket file exists (new listener created)
        assert!(socket_path.exists(), "Socket file should exist");

        // Clean up
        if let Ok((listener, info)) = result {
            drop(listener);
            drop(info);
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_create_listener_enhanced_error_messages() {
        use crate::config::{operational_mode::OperationalMode, Config};
        use tempfile::TempDir;

        // Test permission denied error enhancement
        let temp_dir = TempDir::new().expect("Failed to create temp dir");

        // Create a read-only directory
        let readonly_dir = temp_dir.path().join("readonly");
        std::fs::create_dir(&readonly_dir).expect("Failed to create dir");

        // Set directory to read-only (no write permission)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o444); // read-only
            std::fs::set_permissions(&readonly_dir, perms).expect("Failed to set permissions");
        }

        let socket_path = readonly_dir.join("test.sock");
        let socket_path_str = socket_path.to_str().expect("Invalid path").to_string();

        let mut config = Config::default();
        config.operational_mode = OperationalMode::HttpUds;
        config.trace_agent_uds_path = Some(socket_path_str);

        let result = create_listener(&config).await;
        assert!(result.is_err(), "Should fail due to permission denied");

        if let Err(e) = result {
            // Should have enhanced error message mentioning permissions
            let err_str = e.to_string();
            assert!(
                err_str.contains("Permission denied")
                    || err_str.contains("check directory permissions"),
                "Error should mention permission issue, got: {}",
                err_str
            );
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_cleanup_guard_multiple_references() {
        use crate::config::{operational_mode::OperationalMode, Config};
        use tempfile::TempDir;

        // Test that socket is only cleaned up when LAST Arc reference is dropped
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let custom_path = temp_dir.path().join("multi-ref.sock");
        let custom_path_str = custom_path.to_str().expect("Invalid path").to_string();

        let mut config = Config::default();
        config.operational_mode = OperationalMode::HttpUds;
        config.trace_agent_uds_path = Some(custom_path_str);

        let (listener, info1) = create_listener(&config)
            .await
            .expect("Failed to create listener");

        // Clone ListenerInfo to create additional Arc references
        let info2 = info1.clone();
        let info3 = info1.clone();

        // Socket should exist
        assert!(
            custom_path.exists(),
            "Socket should exist while references alive"
        );

        // Drop first two references - socket should still exist
        drop(info1);
        assert!(
            custom_path.exists(),
            "Socket should exist with 2 references remaining"
        );

        drop(info2);
        assert!(
            custom_path.exists(),
            "Socket should exist with 1 reference remaining"
        );

        // Drop listener (not related to cleanup)
        drop(listener);
        assert!(
            custom_path.exists(),
            "Socket should exist even after listener dropped"
        );

        // Drop final reference - socket should be cleaned up
        drop(info3);

        // Give a moment for the cleanup to happen
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Socket should now be removed
        assert!(
            !custom_path.exists(),
            "Socket should be removed when last reference dropped"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_cleanup_guard_actually_removes_socket() {
        use crate::config::{operational_mode::OperationalMode, Config};
        use tempfile::TempDir;

        // Verify that the cleanup guard actually removes the socket file
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let custom_path = temp_dir.path().join("cleanup-test.sock");
        let custom_path_str = custom_path.to_str().expect("Invalid path").to_string();

        let mut config = Config::default();
        config.operational_mode = OperationalMode::HttpUds;
        config.trace_agent_uds_path = Some(custom_path_str);

        {
            let (listener, info) = create_listener(&config)
                .await
                .expect("Failed to create listener");

            // Socket should exist
            assert!(custom_path.exists(), "Socket should exist");

            // Drop both listener and info
            drop(listener);
            drop(info);
        }

        // Give a moment for cleanup
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Socket should be removed
        assert!(
            !custom_path.exists(),
            "Socket should be removed after all references dropped"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_permission_bits_various_modes() {
        use crate::config::{operational_mode::OperationalMode, Config};
        use std::os::unix::fs::PermissionsExt;
        use tempfile::TempDir;

        // Test various permission modes
        let modes = vec![0o600, 0o660, 0o666, 0o700, 0o770, 0o777];

        for mode in modes {
            let temp_dir = TempDir::new().expect("Failed to create temp dir");
            let custom_path = temp_dir.path().join(format!("perm-{:o}.sock", mode));
            let custom_path_str = custom_path.to_str().expect("Invalid path").to_string();

            let mut config = Config::default();
            config.operational_mode = OperationalMode::HttpUds;
            config.trace_agent_uds_path = Some(custom_path_str);
            config.trace_agent_uds_permissions = mode;

            let (listener, info) = create_listener(&config)
                .await
                .expect(&format!("Failed to create listener with mode {:o}", mode));

            // Verify permissions
            let metadata = std::fs::metadata(&custom_path).expect("Failed to get metadata");
            let actual_mode = metadata.permissions().mode() & 0o777;

            assert_eq!(
                actual_mode, mode,
                "Socket should have {:o} permissions, got {:o}",
                mode, actual_mode
            );

            // Clean up
            drop(listener);
            drop(info);
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_socket_path_with_directory_socket() {
        use crate::config::{operational_mode::OperationalMode, Config};
        use tempfile::TempDir;

        // Test that we properly handle when socket path points to a directory
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let dir_path = temp_dir.path().join("test-dir");

        // Create a directory at the socket path location
        std::fs::create_dir(&dir_path).expect("Failed to create dir");

        let socket_path_str = dir_path.to_str().expect("Invalid path").to_string();

        let mut config = Config::default();
        config.operational_mode = OperationalMode::HttpUds;
        config.trace_agent_uds_path = Some(socket_path_str);

        let result = create_listener(&config).await;

        // Should fail - can't bind to a directory
        assert!(
            result.is_err(),
            "Should fail when socket path is a directory"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_validate_uds_path_special_characters() {
        // Test paths with special characters (valid in Unix but potentially problematic)

        // Path with spaces (valid but discouraged)
        assert!(validate_uds_path("/tmp/my socket.sock").is_ok());

        // Path with dash (common and valid)
        assert!(validate_uds_path("/tmp/my-socket.sock").is_ok());

        // Path with underscore (common and valid)
        assert!(validate_uds_path("/tmp/my_socket.sock").is_ok());

        // Path with numbers (valid)
        assert!(validate_uds_path("/tmp/socket123.sock").is_ok());

        // Path with dot (valid)
        assert!(validate_uds_path("/tmp/my.socket.sock").is_ok());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_create_listener_removes_regular_file() {
        use crate::config::{operational_mode::OperationalMode, Config};
        use tempfile::TempDir;

        // Test that we can remove a regular file (not just socket files)
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let socket_path = temp_dir.path().join("regular-file.sock");
        let socket_path_str = socket_path.to_str().expect("Invalid path").to_string();

        // Create a regular file at the socket path
        std::fs::write(&socket_path, b"regular file content").expect("Failed to create file");

        // Verify it's a regular file
        let metadata = std::fs::metadata(&socket_path).expect("Failed to get metadata");
        assert!(metadata.is_file(), "Should be a regular file");

        let mut config = Config::default();
        config.operational_mode = OperationalMode::HttpUds;
        config.trace_agent_uds_path = Some(socket_path_str);

        let result = create_listener(&config).await;
        assert!(
            result.is_ok(),
            "Should successfully remove regular file and create socket"
        );

        if let Ok((listener, info)) = result {
            // Verify new socket was created
            assert!(socket_path.exists(), "Socket should exist");

            // Clean up
            drop(listener);
            drop(info);
        }
    }
}
