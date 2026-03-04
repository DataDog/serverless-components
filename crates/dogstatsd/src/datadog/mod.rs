// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//!Types to serialize data into the Datadog API

mod intake;
mod series;
mod shipping;

pub use intake::*;
pub use series::*;
pub use shipping::*;
