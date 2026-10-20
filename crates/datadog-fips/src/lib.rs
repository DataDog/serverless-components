// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

#[cfg(feature = "fips")]
mod certificate_distrust;
pub mod reqwest_adapter;
