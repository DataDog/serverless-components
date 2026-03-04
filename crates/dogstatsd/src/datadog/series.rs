// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use serde::{Serialize, Serializer};

#[derive(Debug, Serialize, Clone, Copy)]
/// A single point in time
pub(crate) struct Point {
    /// The time at which the point exists
    pub(crate) timestamp: u64,
    /// The point's value
    pub(crate) value: f64,
}

#[derive(Debug, Serialize, Clone)]
/// A named resource
pub(crate) struct Resource {
    /// The name of this resource
    pub(crate) name: String,
    #[serde(rename = "type")]
    /// The kind of this resource
    pub(crate) kind: String,
}

#[derive(Debug, Clone, Copy)]
/// The kinds of metrics the Datadog API supports
pub(crate) enum DdMetricKind {
    /// An accumulating sum
    Count,
    /// An instantaneous value
    Gauge,
}

impl Serialize for DdMetricKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match *self {
            DdMetricKind::Count => serializer.serialize_u32(0),
            DdMetricKind::Gauge => serializer.serialize_u32(1),
        }
    }
}

#[derive(Debug, Serialize, Clone)]
#[allow(clippy::struct_field_names)]
/// A named collection of `Point` instances.
pub(crate) struct Metric {
    /// The name of the point collection
    pub(crate) metric: &'static str,
    /// The collection of points
    pub(crate) points: [Point; 1],
    /// The resources associated with the points
    pub(crate) resources: Vec<Resource>,
    #[serde(rename = "type")]
    /// The kind of metric
    pub(crate) kind: DdMetricKind,
    pub(crate) tags: Vec<String>,
    /// Optional metadata associated with the metric
    pub(crate) metadata: Option<Metadata>,
}

#[derive(Debug, Serialize, Clone)]
pub struct Metadata {
    pub(crate) origin: Option<Origin>,
}

#[derive(Debug, Serialize, Clone)]
pub struct Origin {
    pub(crate) origin_product: u32,
    pub(crate) origin_sub_product: u32,
    pub(crate) origin_product_detail: u32,
}

#[derive(Debug, Serialize, Clone)]
/// A collection of metrics as defined by the Datadog Metrics API.
// NOTE we have a number of `Vec` instances in this implementation that could
// otherwise be arrays, given that we have constants. Serializing to JSON would
// require us to avoid serializing None or Uninit values, so there's some custom
// work that's needed. For protobuf this more or less goes away.
pub struct Series {
    /// The collection itself
    pub(crate) series: Vec<Metric>,
}

impl Series {
    /// Returns the number of individual metrics in this batch.
    pub fn len(&self) -> usize {
        self.series.len()
    }

    /// Returns true if this batch contains no metrics.
    pub fn is_empty(&self) -> bool {
        self.series.is_empty()
    }
}
