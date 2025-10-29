//! Remote configuration service module facade.
//!
//! This module re-exports the high-level service API while wiring the
//! specialised submodules that implement validation, refresh scheduling,
//! telemetry, and diagnostics.

pub(crate) mod client;
pub(crate) mod config;
mod core;
pub(crate) mod org;
pub(crate) mod refresh;
pub(crate) mod state;
pub(crate) mod telemetry;
#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;
pub(crate) mod util;
pub(crate) mod websocket;

pub use config::ServiceConfig;
pub use core::*;
pub use state::{
    ClientSnapshot, OrgStatusSnapshot, RuntimeStateSnapshot, ServiceSnapshot, WebsocketCheckResult,
};
pub use telemetry::RemoteConfigTelemetry;
