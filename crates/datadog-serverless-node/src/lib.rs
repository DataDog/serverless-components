// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

use napi_derive::napi;

#[napi]
pub fn hello() -> String {
    "Hello from Datadog Serverless Node!".to_string()
}
