//! Tests for backend failure scenarios and retry logic
//!
//! These tests verify that the agent handles various backend failure conditions
//! gracefully without crashing or losing functionality.
//!
//! Note: All FFI-specific tests have been removed as the FFI trace submission
//! functionality (OperationalMode::FfiOnly and submit_traces_msgpack) has been removed.
