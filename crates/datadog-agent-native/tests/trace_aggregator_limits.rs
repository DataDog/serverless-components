//! Tests for trace aggregator limits and memory management
//!
//! These tests verified that the 50MB trace aggregator limit was enforced
//! and that the system handled memory pressure correctly.
//!
//! Note: All tests have been removed as they relied on the FFI trace submission
//! functionality (OperationalMode::FfiOnly and submit_traces_msgpack) which has been removed.
//! Future tests for trace aggregator limits should be written using HTTP submission.
