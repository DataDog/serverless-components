//! Build script for datadog-agent-native.
//!
//! This build script performs the following tasks at compile time:
//!
//! 1. **Version Information**: Sets the agent version as a build-time constant
//! 2. **Build Timestamp**: Records when the agent was built (RFC3339 format)
//! 3. **C Bindings Generation**: Creates a C header file for FFI functions
//!
//! # Generated Artifacts
//!
//! - **Environment Variables**:
//!   - `AGENT_VERSION`: Agent version string (embedded in binary)
//!   - `BUILD_TIMESTAMP`: ISO 8601 timestamp of build time
//!
//! - **C Header File**:
//!   - `datadog_agent_native.h`: C bindings for FFI functions
//!   - Located in the crate root directory
//!   - Generated from Rust code using cbindgen
//!
//! # Version Management
//!
//! The agent version is fixed at 7.70.0 for backend compatibility. This ensures
//! that the agent reports a consistent version to the Datadog backend regardless
//! of the actual crate version. The Datadog backend uses this version for:
//! - Feature compatibility checks
//! - Protocol version negotiation
//! - Telemetry and monitoring
//!
//! # C Bindings
//!
//! The generated C header file (`datadog_agent_native.h`) provides:
//! - Function declarations for all `#[no_mangle] pub extern "C"` functions
//! - Type definitions for FFI-compatible structs
//! - Documentation comments extracted from Rust code
//! - C++ compatibility (`extern "C"` guards)
//! - Include guards to prevent multiple inclusion
//!
//! # Incremental Builds
//!
//! The build script registers dependencies on:
//! - `src/ffi/mod.rs`: FFI function definitions
//! - `cbindgen.toml`: cbindgen configuration
//!
//! Changes to these files will trigger regeneration of the C header.
//!
//! # Example
//!
//! The generated header can be used in C/C++ code like this:
//!
//! ```c
//! #include "datadog_agent_native.h"
//!
//! int main() {
//!     // Call Rust FFI functions from C
//!     AgentHandle* agent = agent_create();
//!     agent_start(agent);
//!     agent_destroy(agent);
//!     return 0;
//! }
//! ```

use std::env;

fn main() {
    // Get the crate root directory from Cargo
    // Decision: Unwrap is safe here because CARGO_MANIFEST_DIR is always set by Cargo during build
    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();

    // Set build-time version information as an environment variable
    // Decision: Use a fixed version (7.70.0) for backend compatibility
    // The Datadog backend expects a stable version for feature compatibility checks
    // rather than the actual crate version which may change frequently
    println!("cargo:rustc-env=AGENT_VERSION=7.70.0");

    // Set build timestamp as an environment variable
    // Decision: Use RFC3339 format (ISO 8601) for standardized timestamp representation
    // This allows easy parsing and display in logs, telemetry, and debugging
    let now = chrono::Utc::now();
    println!("cargo:rustc-env=BUILD_TIMESTAMP={}", now.to_rfc3339());

    // Generate C bindings using cbindgen
    // This creates a C header file (datadog_agent_native.h) from Rust FFI functions
    // Decision: Configure cbindgen for maximum compatibility and safety
    cbindgen::Builder::new()
        .with_crate(crate_dir)
        // Decision: Use C language (not C++) as the base for widest compatibility
        .with_language(cbindgen::Language::C)
        // Decision: Use #pragma once for cleaner header (widely supported by modern compilers)
        .with_pragma_once(true)
        // Decision: Also add traditional include guard as fallback for older compilers
        .with_include_guard("DATADOG_AGENT_NATIVE_H")
        // Decision: Include documentation comments from Rust code in the C header
        // This makes the generated header self-documenting for C/C++ consumers
        .with_documentation(true)
        // Decision: Enable C++ compatibility (extern "C" guards)
        // This allows the header to be used from both C and C++ code
        .with_cpp_compat(true)
        .generate()
        // Decision: Panic if C binding generation fails (build should fail loudly)
        // This prevents shipping a crate without usable FFI bindings
        .expect("Unable to generate C bindings")
        // Decision: Write to crate root (not target/) so the header can be committed to git
        // This allows C/C++ consumers to use the header without building the Rust crate
        .write_to_file("datadog_agent_native.h");

    // Register build script dependencies for incremental builds
    // Decision: Rerun build script if FFI definitions change
    // This ensures the C header stays in sync with Rust FFI code
    println!("cargo:rerun-if-changed=src/ffi/mod.rs");

    // Decision: Rerun build script if cbindgen config changes
    // This ensures header generation respects any config changes
    println!("cargo:rerun-if-changed=cbindgen.toml");
}
