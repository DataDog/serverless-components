// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(not(test), deny(clippy::panic))]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![cfg_attr(not(test), deny(clippy::todo))]
#![cfg_attr(not(test), deny(clippy::unimplemented))]

pub mod azure_cpu;
pub mod azure_instance;
#[cfg(not(windows))]
pub(crate) mod azure_linux;
pub mod azure_tags;
#[cfg(all(windows, feature = "windows-enhanced-metrics"))]
pub(crate) mod azure_windows;
