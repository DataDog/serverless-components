// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! The aggregation of metrics.

use crate::constants;
use crate::datadog::{
    self, Metadata as MetadataToShip, Metric as MetricToShip, Origin as OriginToShip, Series,
};
use crate::errors;
use crate::metric::{self, Metric, MetricValue, SortedTags};
use crate::origin::{OriginCategory, OriginProduct, OriginService};

use datadog_protos::metrics::{Dogsketch, Metadata, Origin, Sketch, SketchPayload};
use ddsketch_agent::DDSketch;
use hashbrown::hash_table;
use protobuf::{Message, MessageField, SpecialFields};
use tracing::{error, warn};
use ustr::Ustr;

/// The length in seconds of the buckets that metrics are aggregated into.
const BUCKET_SECONDS: i64 = 10;

/// How series are encoded for the intake.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SeriesFormat {
    /// Counts and gauges with the type values 0 and 1, and no interval.
    #[default]
    Legacy,
    /// As the Go agent's DogStatsD: a count is a rate per second over its 10-second bucket, a
    /// gauge has the series API v2 gauge type, and both carry the interval.
    Agent,
}

impl MetricValue {
    fn aggregate(&mut self, metric: Metric) {
        // safe because we know there's at least one value when we parse
        // TODO aggregating different types should return error
        match self {
            MetricValue::Count(count) => *count += metric.value.get_value().unwrap_or_default(),
            MetricValue::Gauge(gauge) => *gauge = metric.value.get_value().unwrap_or_default(),
            MetricValue::Distribution(distribution) => {
                if let Some(value) = metric.value.get_sketch() {
                    distribution.merge(value);
                }
            }
            MetricValue::Histogram(histogram) => {
                if let MetricValue::Histogram(other) = metric.value {
                    histogram.samples.extend(other.samples);
                }
            }
            MetricValue::Set(set) => {
                if let MetricValue::Set(other) = metric.value {
                    set.extend(other);
                }
            }
        }
    }

    pub fn get_value(&self) -> Option<f64> {
        match self {
            MetricValue::Count(count) => Some(*count),
            MetricValue::Gauge(gauge) => Some(*gauge),
            MetricValue::Distribution(_) | MetricValue::Histogram(_) | MetricValue::Set(_) => None,
        }
    }

    pub fn get_sketch(&self) -> Option<&DDSketch> {
        match self {
            MetricValue::Distribution(distribution) => Some(distribution),
            _ => None,
        }
    }
}

#[derive(Clone)]
// NOTE by construction we know that intervals and contexts do not explore the
// full space of usize but the type system limits how we can express this today.
pub struct Aggregator {
    tags: SortedTags,
    map: hash_table::HashTable<Metric>,
    max_batch_entries_single_metric: usize,
    max_batch_bytes_single_metric: u64,
    max_batch_entries_sketch_metric: usize,
    max_batch_bytes_sketch_metric: u64,
    max_context: usize,
    series_format: SeriesFormat,
    /// The origin of every metric, in place of the origin found from its name and tags.
    origin: Option<Origin>,
    /// The origin service of metrics named `runtime.*`, in place of the service of `origin`.
    runtime_origin_service: Option<u32>,
    /// Whether a tag in both the base tags and a metric's tags is sent once.
    unique_tags: bool,
}

impl Aggregator {
    /// Create a new instance of `Aggregator`
    ///
    /// # Errors
    ///
    /// Will fail at runtime if the type `INTERVALS` and `CONTEXTS` exceed their
    /// counterparts in `constants`. This would be better as a compile-time
    /// issue, although leaving this open allows for runtime configuration.
    #[allow(clippy::cast_precision_loss)]
    pub fn new(tags: SortedTags, max_context: usize) -> Result<Self, errors::Creation> {
        if max_context > constants::MAX_CONTEXTS {
            return Err(errors::Creation::Contexts);
        }
        Ok(Self {
            tags,
            map: hash_table::HashTable::new(),
            max_batch_entries_single_metric: constants::MAX_ENTRIES_SINGLE_METRIC,
            max_batch_bytes_single_metric: constants::MAX_SIZE_BYTES_SINGLE_METRIC,
            max_batch_entries_sketch_metric: constants::MAX_ENTRIES_SKETCH_METRIC,
            max_batch_bytes_sketch_metric: constants::MAX_SIZE_SKETCH_METRIC,
            max_context,
            series_format: SeriesFormat::default(),
            origin: None,
            runtime_origin_service: None,
            unique_tags: false,
        })
    }

    /// Sets how series are encoded.
    #[must_use]
    pub fn with_series_format(mut self, series_format: SeriesFormat) -> Self {
        self.series_format = series_format;
        self
    }

    /// Sends a tag that is in both the base tags and a metric's tags once, as the Go agent does.
    #[must_use]
    pub fn with_unique_tags(mut self) -> Self {
        self.unique_tags = true;
        self
    }

    /// Gives every metric this origin, in place of the origin found from its name and tags.
    #[must_use]
    pub fn with_origin(
        mut self,
        product: OriginProduct,
        category: OriginCategory,
        service: OriginService,
    ) -> Self {
        self.origin = Some(Origin {
            origin_product: product.into(),
            origin_category: category.into(),
            origin_service: service.into(),
            ..Default::default()
        });
        self
    }

    /// With `with_origin`, gives metrics named `runtime.*` this origin service instead, as the Go
    /// agent's serverless DogStatsD does.
    #[must_use]
    pub fn with_runtime_origin_service(mut self, service: OriginService) -> Self {
        self.runtime_origin_service = Some(service.into());
        self
    }

    /// Insert a `Metric` into the `Aggregator` at the current interval
    ///
    /// # Errors
    ///
    /// Function will return overflow error if more than
    /// `min(constants::MAX_CONTEXTS, CONTEXTS)` is exceeded.
    pub fn insert(&mut self, metric: Metric) -> Result<(), errors::Insert> {
        let id = metric::id(metric.name, &metric.tags, metric.timestamp);
        let len = self.map.len();

        match self.map.entry(
            id,
            |m| m.id == id,
            |m| metric::id(m.name, &m.tags, m.timestamp),
        ) {
            hash_table::Entry::Vacant(entry) => {
                if len >= self.max_context {
                    return Err(errors::Insert::Overflow);
                }
                entry.insert(metric);
            }
            hash_table::Entry::Occupied(mut entry) => {
                entry.get_mut().value.aggregate(metric);
            }
        }
        Ok(())
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    #[must_use]
    pub fn distributions_to_protobuf(&self) -> SketchPayload {
        let mut sketch_payload = SketchPayload::new();

        sketch_payload.sketches = self
            .map
            .iter()
            .filter_map(|entry| match entry.value {
                MetricValue::Distribution(_) => build_sketch(
                    entry,
                    self.tags.clone(),
                    self.unique_tags,
                    origin_override(self.origin.as_ref(), self.runtime_origin_service, entry),
                ),
                _ => None,
            })
            .collect();
        sketch_payload
    }

    #[must_use]
    pub fn consume_distributions(&mut self) -> Vec<SketchPayload> {
        self.consume_distributions_matching(|_| true)
    }

    /// Consumes the distributions whose 10-second bucket has ended at `now`, in seconds since
    /// the Unix epoch, and keeps the others.
    #[must_use]
    pub fn consume_closed_distributions(&mut self, now: i64) -> Vec<SketchPayload> {
        self.consume_distributions_matching(|entry| bucket_closed(entry, now))
    }

    fn consume_distributions_matching(
        &mut self,
        selected: impl Fn(&Metric) -> bool,
    ) -> Vec<SketchPayload> {
        let mut batched_payloads = Vec::new();
        let mut sketch_payload = SketchPayload::new();
        let mut this_batch_size = 0u64;
        for sketch in self
            .map
            .extract_if(|entry| {
                if let MetricValue::Distribution(_) = entry.value {
                    return selected(entry);
                }
                false
            })
            .filter_map(|entry| {
                build_sketch(
                    &entry,
                    self.tags.clone(),
                    self.unique_tags,
                    origin_override(self.origin.as_ref(), self.runtime_origin_service, &entry),
                )
            })
        {
            let next_chunk_size = sketch.compute_size();

            if (sketch_payload.sketches.len() >= self.max_batch_entries_sketch_metric)
                || (this_batch_size + next_chunk_size >= self.max_batch_bytes_sketch_metric)
            {
                if this_batch_size == 0 {
                    warn!(
                        "Only one distribution exceeds max batch size, adding it anyway: {:?} with {}",
                        sketch.metric, next_chunk_size
                    );
                } else {
                    batched_payloads.push(sketch_payload);
                    sketch_payload = SketchPayload::new();
                    this_batch_size = 0u64;
                }
            }
            this_batch_size += next_chunk_size;
            sketch_payload.sketches.push(sketch);
        }
        if !sketch_payload.sketches.is_empty() {
            batched_payloads.push(sketch_payload);
        }
        batched_payloads
    }

    #[must_use]
    pub fn to_series(&self) -> Series {
        let mut series_payload = Series {
            series: Vec::with_capacity(1_024),
        };

        self.map
            .iter()
            .flat_map(|entry| match entry.value {
                MetricValue::Distribution(_) => Vec::new(),
                _ => build_metrics(
                    entry,
                    self.tags.clone(),
                    self.unique_tags,
                    self.series_format,
                    origin_override(self.origin.as_ref(), self.runtime_origin_service, entry),
                ),
            })
            .for_each(|metric| series_payload.series.push(metric));
        series_payload
    }

    #[must_use]
    pub fn consume_metrics(&mut self) -> Vec<Series> {
        self.consume_metrics_matching(|_| true)
    }

    /// Consumes the series metrics whose 10-second bucket has ended at `now`, in seconds
    /// since the Unix epoch, and keeps the others.
    #[must_use]
    pub fn consume_closed_metrics(&mut self, now: i64) -> Vec<Series> {
        self.consume_metrics_matching(|entry| bucket_closed(entry, now))
    }

    fn consume_metrics_matching(&mut self, selected: impl Fn(&Metric) -> bool) -> Vec<Series> {
        let series_format = self.series_format;
        let mut batched_payloads = Vec::new();
        let mut series_payload = Series {
            series: Vec::with_capacity(1_024),
        };
        let mut this_batch_size = 0u64;
        for metric in self
            .map
            .extract_if(|entry| {
                if let MetricValue::Distribution(_) = entry.value {
                    return false;
                }
                selected(entry)
            })
            .flat_map(|entry| {
                build_metrics(
                    &entry,
                    self.tags.clone(),
                    self.unique_tags,
                    series_format,
                    origin_override(self.origin.as_ref(), self.runtime_origin_service, &entry),
                )
            })
        {
            // TODO serialization is made twice for each point. If we return a Vec<u8> we can avoid
            // that
            let serialized_metric_size = match serde_json::to_vec(&metric) {
                Ok(serialized_metric) => serialized_metric.len() as u64,
                Err(e) => {
                    error!("failed to serialize metric: {:?}", e);
                    0u64
                }
            };

            if serialized_metric_size > 0 {
                if (series_payload.series.len() >= self.max_batch_entries_single_metric)
                    || (this_batch_size + serialized_metric_size
                        >= self.max_batch_bytes_single_metric)
                {
                    if this_batch_size == 0 {
                        warn!(
                            "Only one metric exceeds max batch size, adding it anyway: {:?} with {}",
                            metric.metric, serialized_metric_size
                        );
                    } else {
                        batched_payloads.push(series_payload);
                        series_payload = Series {
                            series: Vec::with_capacity(1_024),
                        };
                        this_batch_size = 0u64;
                    }
                }
                series_payload.series.push(metric);
                this_batch_size += serialized_metric_size;
            }
        }

        if !series_payload.series.is_empty() {
            batched_payloads.push(series_payload);
        }
        batched_payloads
    }

    pub fn get_entry_by_id(
        &self,
        name: Ustr,
        tags: &Option<SortedTags>,
        timestamp: i64,
    ) -> Option<&Metric> {
        let id = metric::id(name, tags, timestamp);
        self.map.find(id, |m| m.id == id)
    }
}

fn add_metric_tags(base_tag_vec: &mut SortedTags, tags: &SortedTags, unique_tags: bool) {
    if unique_tags {
        base_tag_vec.extend_unique(tags);
    } else {
        base_tag_vec.extend(tags);
    }
}

/// The origin that replaces the one found from the metric's name and tags, if any.
fn origin_override(
    origin: Option<&Origin>,
    runtime_origin_service: Option<u32>,
    entry: &Metric,
) -> Option<Origin> {
    let mut origin = origin?.clone();
    if let Some(service) = runtime_origin_service
        && entry.name.starts_with("runtime.")
    {
        origin.origin_service = service;
    }
    Some(origin)
}

fn build_sketch(
    entry: &Metric,
    mut base_tag_vec: SortedTags,
    unique_tags: bool,
    origin_override: Option<Origin>,
) -> Option<Sketch> {
    let sketch = entry.value.get_sketch()?;
    let mut dogsketch = Dogsketch::default();
    sketch.merge_to_dogsketch(&mut dogsketch);
    // TODO(Astuyve) allow users to specify timestamp
    dogsketch.set_ts(entry.timestamp);
    let mut sketch = Sketch::default();
    sketch.set_dogsketches(vec![dogsketch]);
    let name = entry.name.to_string();
    sketch.set_metric(name.clone().into());
    if let Some(tags) = &entry.tags {
        add_metric_tags(&mut base_tag_vec, tags, unique_tags);
    }
    sketch.set_tags(base_tag_vec.to_chars());

    let origin = origin_override.or_else(|| entry.find_origin(base_tag_vec));
    if let Some(origin) = origin {
        sketch.set_metadata(Metadata {
            origin: MessageField::some(origin),
            special_fields: SpecialFields::default(),
        });
    }

    Some(sketch)
}

fn bucket_closed(entry: &Metric, now: i64) -> bool {
    entry.timestamp + BUCKET_SECONDS <= now
}

/// A series value, before it is encoded in the series format.
enum SeriesValue {
    Count(f64),
    Gauge(f64),
}

/// The series of a metric, each as (name suffix, value).
fn series_values(value: &MetricValue) -> Vec<(&'static str, SeriesValue)> {
    match value {
        MetricValue::Count(count) => vec![("", SeriesValue::Count(*count))],
        MetricValue::Gauge(gauge) => vec![("", SeriesValue::Gauge(*gauge))],
        #[allow(clippy::cast_precision_loss)]
        MetricValue::Set(set) => vec![("", SeriesValue::Gauge(set.len() as f64))],
        MetricValue::Histogram(histogram) => histogram_series(histogram),
        MetricValue::Distribution(_) => Vec::new(),
    }
}

/// The Go agent's default histogram series: the aggregates `max`, `median`, `avg` and `count`,
/// and the 95th percentile. `count` is the weighted number of samples.
fn histogram_series(histogram: &metric::Histogram) -> Vec<(&'static str, SeriesValue)> {
    let mut samples = histogram.samples.clone();
    samples.sort_by(|a, b| a.0.total_cmp(&b.0));
    let Some(&(max, _)) = samples.last() else {
        return Vec::new();
    };
    let count: f64 = samples.iter().map(|(_, weight)| weight).sum();
    let sum: f64 = samples.iter().map(|(value, weight)| value * weight).sum();
    // The first value whose cumulative weight passes the target rank.
    let at_rank = |target: f64| {
        let mut cumulative = 0.0;
        samples
            .iter()
            .find(|(_, weight)| {
                cumulative += weight;
                cumulative > target
            })
            .map_or(max, |(value, _)| *value)
    };
    vec![
        (".max", SeriesValue::Gauge(max)),
        (".median", SeriesValue::Gauge(at_rank((count - 1.0) / 2.0))),
        (".avg", SeriesValue::Gauge(sum / count)),
        (".count", SeriesValue::Count(count)),
        (
            ".95percentile",
            SeriesValue::Gauge(at_rank((95.0 * count - 1.0) / 100.0)),
        ),
    ]
}

fn build_metrics(
    entry: &Metric,
    mut base_tag_vec: SortedTags,
    unique_tags: bool,
    series_format: SeriesFormat,
    origin_override: Option<Origin>,
) -> Vec<MetricToShip> {
    let resources;
    if let Some(tags) = entry.tags.clone() {
        resources = tags.to_resources();
    } else {
        resources = Vec::new();
    }

    if let Some(tags) = &entry.tags {
        add_metric_tags(&mut base_tag_vec, tags, unique_tags);
    }

    let origin = origin_override.or_else(|| entry.find_origin(base_tag_vec.clone()));
    let metadata = origin.map(|o| MetadataToShip {
        origin: Some(OriginToShip {
            origin_product: o.origin_product,
            origin_sub_product: o.origin_category,
            origin_product_detail: o.origin_service,
        }),
    });
    let tags = base_tag_vec.to_strings();

    series_values(&entry.value)
        .into_iter()
        .map(|(suffix, value)| {
            let (kind, value, interval) = match (series_format, value) {
                (SeriesFormat::Legacy, SeriesValue::Count(value)) => {
                    (datadog::DdMetricKind::Count, value, None)
                }
                (SeriesFormat::Legacy, SeriesValue::Gauge(value)) => {
                    (datadog::DdMetricKind::Gauge, value, None)
                }
                (SeriesFormat::Agent, SeriesValue::Count(value)) => (
                    datadog::DdMetricKind::V2Rate,
                    value / BUCKET_SECONDS as f64,
                    Some(BUCKET_SECONDS as u64),
                ),
                (SeriesFormat::Agent, SeriesValue::Gauge(value)) => (
                    datadog::DdMetricKind::V2Gauge,
                    value,
                    Some(BUCKET_SECONDS as u64),
                ),
            };
            let metric = if suffix.is_empty() {
                entry.name.as_str()
            } else {
                Ustr::from(&format!("{}{suffix}", entry.name)).as_str()
            };
            MetricToShip {
                metric,
                resources: resources.clone(),
                kind,
                points: [datadog::Point {
                    value,
                    // TODO(astuyve) allow user to specify timestamp
                    timestamp: entry.timestamp as u64,
                }],
                tags: tags.clone(),
                metadata: metadata.clone(),
                interval,
            }
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub mod tests {
    use crate::aggregator::Aggregator;
    use crate::metric;
    use crate::metric::{EMPTY_TAGS, SortedTags, parse};
    use datadog_protos::metrics::SketchPayload;
    use hashbrown::hash_table;
    use protobuf::Message;
    use std::sync::Mutex;

    const PRECISION: f64 = 0.000_000_01;

    const SINGLE_METRIC_SIZE: usize = 209; // taken from the test, size of a serialized metric with one tag and 1 digit counter value
    const SINGLE_DISTRIBUTION_SIZE: u64 = 140;
    const DEFAULT_TAGS: &str =
        "dd_extension_version:63-next,architecture:x86_64,_dd.compute_stats:1";

    pub fn assert_value(
        aggregator_mutex: &Mutex<Aggregator>,
        metric_id: &str,
        value: f64,
        tags: &str,
        timestamp: i64,
    ) {
        let aggregator = aggregator_mutex.lock().unwrap();
        if let Some(e) = aggregator.get_entry_by_id(
            metric_id.into(),
            &Some(SortedTags::parse(tags).unwrap()),
            timestamp,
        ) {
            let metric = e.value.get_value().unwrap();
            assert!((metric - value).abs() < PRECISION);
        } else {
            panic!("{}", format!("{metric_id} not found"));
        }
    }

    pub fn assert_sketch(
        aggregator_mutex: &Mutex<Aggregator>,
        metric_id: &str,
        value: f64,
        timestamp: i64,
    ) {
        let aggregator = aggregator_mutex.lock().unwrap();
        if let Some(e) = aggregator.get_entry_by_id(metric_id.into(), &None, timestamp) {
            let metric = e.value.get_sketch().unwrap();
            assert!((metric.max().unwrap() - value).abs() < PRECISION);
            assert!((metric.min().unwrap() - value).abs() < PRECISION);
            assert!((metric.sum().unwrap() - value).abs() < PRECISION);
            assert!((metric.avg().unwrap() - value).abs() < PRECISION);
        } else {
            panic!("{}", format!("{metric_id} not found"));
        }
    }

    #[test]
    fn insertion() {
        let mut aggregator = Aggregator::new(EMPTY_TAGS, 2).unwrap();

        let metric1 = parse("test:1|c|#k:v").expect("metric parse failed");
        let metric2 = parse("foo:1|c|#k:v").expect("metric parse failed");

        assert!(aggregator.insert(metric1).is_ok());
        assert!(aggregator.insert(metric2).is_ok());

        // Both unique contexts get one slot.
        assert_eq!(aggregator.map.len(), 2);
    }

    #[test]
    fn distribution_insertion() {
        let mut aggregator = Aggregator::new(EMPTY_TAGS, 2).unwrap();

        let metric1 = parse("test:1|d|#k:v").expect("metric parse failed");
        let metric2 = parse("foo:1|d|#k:v").expect("metric parse failed");

        assert!(aggregator.insert(metric1).is_ok());
        assert!(aggregator.insert(metric2).is_ok());

        // Both unique contexts get one slot.
        assert_eq!(aggregator.map.len(), 2);
    }

    #[test]
    fn overflow() {
        let mut aggregator = Aggregator::new(EMPTY_TAGS, 2).unwrap();
        let mut now = std::time::UNIX_EPOCH
            .elapsed()
            .expect("unable to poll clock, unrecoverable")
            .as_secs()
            .try_into()
            .unwrap_or_default();
        now = (now / 10) * 10;
        let metric1 = parse("test:1|c|#k:v").expect("metric parse failed");
        let metric2 = parse("foo:1|c|#k:v").expect("metric parse failed");
        let metric3 = parse("bar:1|c|#k:v").expect("metric parse failed");

        let id1 = metric::id(metric1.name, &metric1.tags, now);
        let id2 = metric::id(metric2.name, &metric2.tags, now);
        let id3 = metric::id(metric3.name, &metric3.tags, now);

        assert_ne!(id1, id2);
        assert_ne!(id1, id3);
        assert_ne!(id2, id3);

        assert!(aggregator.insert(metric1).is_ok());
        assert_eq!(aggregator.map.len(), 1);

        assert!(aggregator.insert(metric2.clone()).is_ok());
        assert!(aggregator.insert(metric2.clone()).is_ok());
        assert!(aggregator.insert(metric2).is_ok());
        assert_eq!(aggregator.map.len(), 2);

        assert!(aggregator.insert(metric3).is_err());
        assert_eq!(aggregator.map.len(), 2);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn clear() {
        let mut aggregator = Aggregator::new(EMPTY_TAGS, 2).unwrap();
        let mut now = 1656581409;
        now = (now / 10) * 10;
        let metric1 = parse("test:3|c|#k1:v1|T1656581409").expect("metric parse failed");
        let metric2 = parse("foo:5|c|#k2:v2|T1656581409").expect("metric parse failed");

        assert!(aggregator.insert(metric1).is_ok());
        assert!(aggregator.insert(metric2).is_ok());

        assert_eq!(aggregator.map.len(), 2);
        if let Some(v) = aggregator.get_entry_by_id(
            "foo".into(),
            &Some(SortedTags::parse("k2:v2").unwrap()),
            now,
        ) {
            assert_eq!(v.value.get_value().unwrap(), 5f64);
        } else {
            panic!("failed to get value by id");
        }

        if let Some(v) = aggregator.get_entry_by_id(
            "test".into(),
            &Some(SortedTags::parse("k1:v1").unwrap()),
            now,
        ) {
            assert_eq!(v.value.get_value().unwrap(), 3f64);
        } else {
            panic!("failed to get value by id");
        }

        aggregator.clear();
        assert_eq!(aggregator.map.len(), 0);
    }

    #[test]
    fn to_series() {
        let mut aggregator = Aggregator::new(EMPTY_TAGS, 2).unwrap();

        let metric1 = parse("test:1|c|#k1:v1,k2:v2").expect("metric parse failed");
        let metric2 = parse("foo:1|c|#k:v").expect("metric parse failed");
        let metric3 = parse("bar:1|c|#k:v").expect("metric parse failed");

        assert!(aggregator.insert(metric1).is_ok());
        assert!(aggregator.insert(metric2).is_ok());

        assert_eq!(aggregator.map.len(), 2);
        assert_eq!(aggregator.to_series().len(), 2);
        // to_series should not mutate the state
        assert_eq!(aggregator.map.len(), 2);
        assert_eq!(aggregator.to_series().len(), 2);
        assert_eq!(aggregator.map.len(), 2);

        assert!(aggregator.insert(metric3).is_err());
        assert_eq!(aggregator.to_series().len(), 2);
    }

    /// By default a tag in both the base tags and a metric's tags can be sent twice; with unique
    /// tags it is sent once.
    #[test]
    fn base_and_metric_tags_table() {
        for (unique_tags, expected) in [
            (
                false,
                vec!["env:prod", "env:prod", "local:true", "service:app"],
            ),
            (true, vec!["env:prod", "local:true", "service:app"]),
        ] {
            let mut aggregator =
                Aggregator::new(SortedTags::parse("env:prod,local:true").unwrap(), 2).unwrap();
            if unique_tags {
                aggregator = aggregator.with_unique_tags();
            }
            for metric in [
                "test:1|c|#env:prod,service:app",
                "dist:1|d|#env:prod,service:app",
            ] {
                aggregator.insert(parse(metric).unwrap()).unwrap();
            }

            assert_eq!(
                aggregator.to_series().series[0].tags,
                expected,
                "unique tags {unique_tags}"
            );
            let sketch_tags: Vec<String> = aggregator.distributions_to_protobuf().sketches()[0]
                .tags
                .iter()
                .map(ToString::to_string)
                .collect();
            assert_eq!(sketch_tags, expected, "unique tags {unique_tags}");
        }
    }

    #[test]
    fn distributions_to_protobuf() {
        let mut aggregator = Aggregator::new(EMPTY_TAGS, 2).unwrap();

        let metric1 = parse("test:1|d|#k:v").expect("metric parse failed");
        let metric2 = parse("foo:1|d|#k:v").expect("metric parse failed");

        assert!(aggregator.insert(metric1).is_ok());
        assert!(aggregator.insert(metric2).is_ok());

        assert_eq!(aggregator.map.len(), 2);
        assert_eq!(aggregator.distributions_to_protobuf().sketches().len(), 2);
        assert_eq!(aggregator.map.len(), 2);
        assert_eq!(aggregator.distributions_to_protobuf().sketches().len(), 2);
        assert_eq!(aggregator.map.len(), 2);
    }

    #[test]
    fn consume_distributions_ignore_single_metrics() {
        let mut aggregator = Aggregator::new(EMPTY_TAGS, 1_000).unwrap();
        assert_eq!(aggregator.distributions_to_protobuf().sketches.len(), 0);

        assert!(
            aggregator
                .insert(parse("test1:1|d|#k:v".to_string().as_str()).expect("metric parse failed"))
                .is_ok()
        );
        assert_eq!(aggregator.distributions_to_protobuf().sketches.len(), 1);

        assert!(
            aggregator
                .insert(parse("foo:1|c|#k:v").expect("metric parse failed"))
                .is_ok()
        );
        assert_eq!(aggregator.distributions_to_protobuf().sketches.len(), 1);
    }

    #[test]
    fn consume_distributions_batch_entries() {
        let max_batch = 5;
        let tot = 12;
        let mut aggregator = Aggregator {
            tags: EMPTY_TAGS,
            map: hash_table::HashTable::new(),
            max_batch_entries_single_metric: 1_000,
            max_batch_bytes_single_metric: 1_000,
            max_batch_entries_sketch_metric: max_batch,
            max_batch_bytes_sketch_metric: 1_500,
            max_context: 1_000,
            series_format: crate::aggregator::SeriesFormat::Legacy,
            origin: None,
            runtime_origin_service: None,
            unique_tags: false,
        };

        add_metrics(tot, &mut aggregator, "d".to_string());
        let batched = aggregator.consume_distributions();
        assert_eq!(aggregator.consume_distributions().len(), 0);

        assert_eq!(batched.len(), 3);
        assert_eq!(batched.first().unwrap().sketches.len(), max_batch);
        assert_eq!(batched.get(1).unwrap().sketches.len(), max_batch);
        assert_eq!(batched.get(2).unwrap().sketches.len(), tot - max_batch * 2);
    }

    #[test]
    fn consume_distributions_batch_bytes() {
        let expected_distribution_per_batch = 2;
        let total_number_of_distributions = 5;
        let max_bytes = SINGLE_DISTRIBUTION_SIZE * expected_distribution_per_batch as u64;
        let mut aggregator = Aggregator {
            tags: to_sorted_tags(),
            map: hash_table::HashTable::new(),
            max_batch_entries_single_metric: 1_000,
            max_batch_bytes_single_metric: 1_000,
            max_batch_entries_sketch_metric: 1_000,
            max_batch_bytes_sketch_metric: max_bytes,
            max_context: 1_000,
            series_format: crate::aggregator::SeriesFormat::Legacy,
            origin: None,
            runtime_origin_service: None,
            unique_tags: false,
        };

        add_metrics(
            total_number_of_distributions,
            &mut aggregator,
            "d".to_string(),
        );
        let batched = aggregator.consume_distributions();

        assert_eq!(
            batched.len(),
            total_number_of_distributions / expected_distribution_per_batch + 1
        );
        assert_eq!(
            batched.first().unwrap().compute_size(),
            SINGLE_DISTRIBUTION_SIZE * expected_distribution_per_batch as u64
        );
        assert_eq!(
            batched.get(1).unwrap().compute_size(),
            SINGLE_DISTRIBUTION_SIZE * expected_distribution_per_batch as u64
        );
        assert_eq!(
            batched.get(2).unwrap().compute_size(),
            SINGLE_DISTRIBUTION_SIZE
        );
    }

    fn to_sorted_tags() -> SortedTags {
        SortedTags::parse(DEFAULT_TAGS).unwrap()
    }

    #[test]
    fn consume_distribution_one_element_bigger_than_max_size() {
        let max_bytes = 1;
        let tot = 5;
        let mut aggregator = Aggregator {
            tags: to_sorted_tags(),
            map: hash_table::HashTable::new(),
            max_batch_entries_single_metric: 1_000,
            max_batch_bytes_single_metric: 1_000,
            max_batch_entries_sketch_metric: 1_000,
            max_batch_bytes_sketch_metric: max_bytes,
            max_context: 1_000,
            series_format: crate::aggregator::SeriesFormat::Legacy,
            origin: None,
            runtime_origin_service: None,
            unique_tags: false,
        };

        add_metrics(tot, &mut aggregator, "d".to_string());
        let batched = aggregator.consume_distributions();

        assert_eq!(batched.len(), tot);
        for a_batch in batched {
            assert_eq!(a_batch.compute_size(), SINGLE_DISTRIBUTION_SIZE);
        }
    }

    fn add_metrics(tot: usize, aggregator: &mut Aggregator, counter_or_distro: String) {
        for i in 1..=tot {
            assert!(
                aggregator
                    .insert(
                        parse(format!("test{i}:{i}|{counter_or_distro}|#k:v").as_str())
                            .expect("metric parse failed")
                    )
                    .is_ok()
            );
        }
    }

    #[test]
    fn consume_series_ignore_distribution() {
        let mut aggregator = Aggregator::new(EMPTY_TAGS, 1_000).unwrap();

        assert_eq!(aggregator.consume_metrics().len(), 0);

        assert!(
            aggregator
                .insert(parse("test1:1|c|#k:v".to_string().as_str()).expect("metric parse failed"))
                .is_ok()
        );
        assert_eq!(aggregator.consume_distributions().len(), 0);
        assert_eq!(aggregator.consume_metrics().len(), 1);
        assert_eq!(aggregator.consume_metrics().len(), 0);

        assert!(
            aggregator
                .insert(parse("test1:1|c|#k:v".to_string().as_str()).expect("metric parse failed"))
                .is_ok()
        );
        assert!(
            aggregator
                .insert(parse("foo:1|d|#k:v").expect("metric parse failed"))
                .is_ok()
        );
        assert_eq!(aggregator.consume_metrics().len(), 1);
        assert_eq!(aggregator.consume_distributions().len(), 1);
        assert_eq!(aggregator.consume_distributions().len(), 0);
    }

    #[test]
    fn consume_series_batch_entries() {
        let max_batch = 5;
        let tot = 13;
        let mut aggregator = Aggregator {
            tags: EMPTY_TAGS,
            map: hash_table::HashTable::new(),
            max_batch_entries_single_metric: max_batch,
            max_batch_bytes_single_metric: 10_000,
            max_batch_entries_sketch_metric: 1_000,
            max_batch_bytes_sketch_metric: 1_500,
            max_context: 1_000,
            series_format: crate::aggregator::SeriesFormat::Legacy,
            origin: None,
            runtime_origin_service: None,
            unique_tags: false,
        };

        add_metrics(tot, &mut aggregator, "c".to_string());

        let batched = aggregator.consume_metrics();
        assert_eq!(batched.len(), 3);
        assert_eq!(batched.first().unwrap().series.len(), max_batch);
        assert_eq!(batched.get(1).unwrap().series.len(), max_batch);
        assert_eq!(batched.get(2).unwrap().series.len(), tot - max_batch * 2);

        assert_eq!(aggregator.consume_metrics().len(), 0);
    }

    #[test]
    fn consume_metrics_batch_bytes() {
        let expected_metrics_per_batch = 2;
        let total_number_of_metrics = 5;
        let two_metrics_size = 406;
        let max_bytes = SINGLE_METRIC_SIZE * expected_metrics_per_batch + 13;
        let mut aggregator = Aggregator {
            tags: to_sorted_tags(),
            map: hash_table::HashTable::new(),
            max_batch_entries_single_metric: 1_000,
            max_batch_bytes_single_metric: max_bytes as u64,
            max_batch_entries_sketch_metric: 1_000,
            max_batch_bytes_sketch_metric: 1_000,
            max_context: 1_000,
            series_format: crate::aggregator::SeriesFormat::Legacy,
            origin: None,
            runtime_origin_service: None,
            unique_tags: false,
        };

        add_metrics(total_number_of_metrics, &mut aggregator, "c".to_string());
        let batched = aggregator.consume_metrics();

        assert_eq!(
            batched.len(),
            total_number_of_metrics / expected_metrics_per_batch + 1
        );
        assert_eq!(
            serde_json::to_vec(batched.first().unwrap()).unwrap().len(),
            two_metrics_size
        );
        assert_eq!(
            serde_json::to_vec(batched.get(1).unwrap()).unwrap().len(),
            two_metrics_size
        );
        assert_eq!(
            serde_json::to_vec(batched.get(2).unwrap()).unwrap().len(),
            SINGLE_METRIC_SIZE
        );
    }

    #[test]
    fn consume_series_one_element_bigger_than_max_size() {
        let max_bytes = 1;
        let tot = 5;
        let mut aggregator = Aggregator {
            tags: to_sorted_tags(),
            map: hash_table::HashTable::new(),
            max_batch_entries_single_metric: 1_000,
            max_batch_bytes_single_metric: max_bytes,
            max_batch_entries_sketch_metric: 1_000,
            max_batch_bytes_sketch_metric: 1_000,
            max_context: 1_000,
            series_format: crate::aggregator::SeriesFormat::Legacy,
            origin: None,
            runtime_origin_service: None,
            unique_tags: false,
        };

        add_metrics(tot, &mut aggregator, "c".to_string());
        let batched = aggregator.consume_metrics();

        assert_eq!(batched.len(), tot);
        for a_batch in batched {
            assert_eq!(
                serde_json::to_vec(&a_batch).unwrap().len(),
                SINGLE_METRIC_SIZE
            );
        }
    }

    #[test]
    fn distribution_serialized_deserialized() {
        let mut aggregator = Aggregator::new(EMPTY_TAGS, 1_000).unwrap();

        add_metrics(10, &mut aggregator, "d".to_string());
        let distribution = aggregator.distributions_to_protobuf();
        assert_eq!(distribution.sketches().len(), 10);

        let serialized = distribution
            .write_to_bytes()
            .expect("Can't serialized proto");

        let deserialized =
            SketchPayload::parse_from_bytes(serialized.as_slice()).expect("failed to parse proto");

        assert_eq!(deserialized.sketches().len(), 10);
        assert_eq!(deserialized, distribution);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn count_aggregation_sums_values() {
        let mut aggregator = Aggregator::new(EMPTY_TAGS, 10).unwrap();

        let m1 = parse("hits:3|c|#env:prod|T1656581409").expect("parse failed");
        let m2 = parse("hits:7|c|#env:prod|T1656581409").expect("parse failed");
        let m3 = parse("hits:5|c|#env:prod|T1656581409").expect("parse failed");

        aggregator.insert(m1).unwrap();
        aggregator.insert(m2).unwrap();
        aggregator.insert(m3).unwrap();

        // Same name + tags + timestamp bucket = one context, values summed
        assert_eq!(aggregator.map.len(), 1);

        let entry = aggregator
            .get_entry_by_id(
                "hits".into(),
                &Some(SortedTags::parse("env:prod").unwrap()),
                1656581400,
            )
            .unwrap();
        assert_eq!(entry.value.get_value().unwrap(), 15.0);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn gauge_aggregation_last_wins() {
        let mut aggregator = Aggregator::new(EMPTY_TAGS, 10).unwrap();

        let m1 = parse("cpu:30.0|g|#host:a|T1656581409").expect("parse failed");
        let m2 = parse("cpu:55.0|g|#host:a|T1656581409").expect("parse failed");
        let m3 = parse("cpu:42.0|g|#host:a|T1656581409").expect("parse failed");

        aggregator.insert(m1).unwrap();
        aggregator.insert(m2).unwrap();
        aggregator.insert(m3).unwrap();

        // Same context, gauge = last value wins
        assert_eq!(aggregator.map.len(), 1);

        let entry = aggregator
            .get_entry_by_id(
                "cpu".into(),
                &Some(SortedTags::parse("host:a").unwrap()),
                1656581400,
            )
            .unwrap();
        assert_eq!(entry.value.get_value().unwrap(), 42.0);
    }

    #[test]
    fn distribution_aggregation_merges_sketches() {
        let mut aggregator = Aggregator::new(EMPTY_TAGS, 10).unwrap();

        let m1 = parse("latency:10.0|d|#svc:web|T1656581409").expect("parse failed");
        let m2 = parse("latency:20.0|d|#svc:web|T1656581409").expect("parse failed");
        let m3 = parse("latency:30.0|d|#svc:web|T1656581409").expect("parse failed");

        aggregator.insert(m1).unwrap();
        aggregator.insert(m2).unwrap();
        aggregator.insert(m3).unwrap();

        // Same context, distributions merge into one sketch
        assert_eq!(aggregator.map.len(), 1);

        let entry = aggregator
            .get_entry_by_id(
                "latency".into(),
                &Some(SortedTags::parse("svc:web").unwrap()),
                1656581400,
            )
            .unwrap();
        let sketch = entry.value.get_sketch().unwrap();
        assert!((sketch.min().unwrap() - 10.0).abs() < PRECISION);
        assert!((sketch.max().unwrap() - 30.0).abs() < PRECISION);
        assert!((sketch.sum().unwrap() - 60.0).abs() < PRECISION);
        assert_eq!(sketch.count(), 3);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn mixed_metric_types_stay_separate() {
        let mut aggregator = Aggregator::new(EMPTY_TAGS, 10).unwrap();

        // Same name but different types and tags keep different contexts
        let count = parse("req:1|c|#route:a|T1656581409").expect("parse failed");
        let gauge = parse("req:5|g|#route:b|T1656581409").expect("parse failed");
        let dist = parse("req:100|d|#route:c|T1656581409").expect("parse failed");

        aggregator.insert(count).unwrap();
        aggregator.insert(gauge).unwrap();
        aggregator.insert(dist).unwrap();

        assert_eq!(aggregator.map.len(), 3);

        let series = aggregator.to_series();
        assert_eq!(series.len(), 2); // count + gauge

        let protos = aggregator.distributions_to_protobuf();
        assert_eq!(protos.sketches().len(), 1); // distribution
    }

    #[test]
    fn series_format_table() {
        use crate::aggregator::SeriesFormat;
        use serde_json::json;

        let cases = [
            (
                SeriesFormat::Legacy,
                "hits:2|c|T1656581405",
                json!({"type": 0, "value": 2.0, "interval": null}),
            ),
            (
                SeriesFormat::Legacy,
                "level:7|g|T1656581405",
                json!({"type": 1, "value": 7.0, "interval": null}),
            ),
            (
                SeriesFormat::Agent,
                "hits:2|c|T1656581405",
                json!({"type": 2, "value": 0.2, "interval": 10}),
            ),
            (
                SeriesFormat::Agent,
                "level:7|g|T1656581405",
                json!({"type": 3, "value": 7.0, "interval": 10}),
            ),
        ];
        for (format, line, expected) in cases {
            let mut aggregator = Aggregator::new(EMPTY_TAGS, 10)
                .unwrap()
                .with_series_format(format);
            aggregator.insert(parse(line).unwrap()).unwrap();
            let series = aggregator.consume_metrics();
            let metric = &serde_json::to_value(&series[0]).unwrap()["series"][0];
            let actual = json!({
                "type": metric["type"],
                "value": metric["points"][0]["value"],
                "interval": metric.get("interval"),
            });
            assert_eq!(actual, expected, "format {format:?}, line {line}");
        }
    }

    #[test]
    fn consume_closed_buckets_keeps_open_buckets() {
        let mut aggregator = Aggregator::new(EMPTY_TAGS, 10).unwrap();
        for line in [
            "hits:1|c|T1656581405",
            "hits:1|c|T1656581415",
            "latency:1|d|T1656581405",
            "latency:1|d|T1656581415",
        ] {
            aggregator.insert(parse(line).unwrap()).unwrap();
        }
        let series_timestamps = |series: Vec<crate::datadog::Series>| -> Vec<u64> {
            series
                .iter()
                .flat_map(|batch| {
                    serde_json::to_value(batch).unwrap()["series"]
                        .clone()
                        .as_array()
                        .unwrap()
                        .clone()
                })
                .map(|metric| metric["points"][0]["timestamp"].as_u64().unwrap())
                .collect()
        };
        let sketch_timestamps = |payloads: Vec<SketchPayload>| -> Vec<i64> {
            payloads
                .iter()
                .flat_map(|payload| payload.sketches.iter())
                .flat_map(|sketch| sketch.dogsketches.iter())
                .map(|dogsketch| dogsketch.ts)
                .collect()
        };

        // The bucket that starts at 1656581410 is still open one second before it ends.
        assert_eq!(
            series_timestamps(aggregator.consume_closed_metrics(1656581419)),
            vec![1656581400]
        );
        assert_eq!(
            sketch_timestamps(aggregator.consume_closed_distributions(1656581419)),
            vec![1656581400]
        );
        assert_eq!(aggregator.map.len(), 2);

        // It is closed when it ends.
        assert_eq!(
            series_timestamps(aggregator.consume_closed_metrics(1656581420)),
            vec![1656581410]
        );
        assert_eq!(
            sketch_timestamps(aggregator.consume_closed_distributions(1656581420)),
            vec![1656581410]
        );
        assert_eq!(aggregator.map.len(), 0);
    }
    #[test]
    fn extended_types_flush_with_histogram_count_as_rate_table() {
        use crate::aggregator::SeriesFormat;
        use crate::metric::{ParseOptions, parse_with_options};
        use serde_json::json;

        let cases = [
            (
                SeriesFormat::Agent,
                vec!["n:5|h", "n:15|h"],
                json!([
                    ["n.95percentile", 3, 15.0, 10],
                    ["n.avg", 3, 10.0, 10],
                    ["n.count", 2, 0.2, 10],
                    ["n.max", 3, 15.0, 10],
                    ["n.median", 3, 5.0, 10],
                ]),
            ),
            (
                SeriesFormat::Agent,
                vec!["n:3|h", "n:1|h", "n:5|h", "n:2|h", "n:4|h"],
                json!([
                    ["n.95percentile", 3, 5.0, 10],
                    ["n.avg", 3, 3.0, 10],
                    ["n.count", 2, 0.5, 10],
                    ["n.max", 3, 5.0, 10],
                    ["n.median", 3, 3.0, 10],
                ]),
            ),
            (
                SeriesFormat::Agent,
                vec!["n:1|h|@0.5", "n:3|h|@0.5"],
                json!([
                    ["n.95percentile", 3, 3.0, 10],
                    ["n.avg", 3, 2.0, 10],
                    ["n.count", 2, 0.4, 10],
                    ["n.max", 3, 3.0, 10],
                    ["n.median", 3, 1.0, 10],
                ]),
            ),
            (
                SeriesFormat::Agent,
                vec!["t:250|ms"],
                json!([
                    ["t.95percentile", 3, 250.0, 10],
                    ["t.avg", 3, 250.0, 10],
                    ["t.count", 2, 0.1, 10],
                    ["t.max", 3, 250.0, 10],
                    ["t.median", 3, 250.0, 10],
                ]),
            ),
            (
                SeriesFormat::Agent,
                vec!["u:alice|s", "u:bob|s", "u:alice|s"],
                json!([["u", 3, 2.0, 10]]),
            ),
            (
                SeriesFormat::Legacy,
                vec!["n:5|h", "u:alice|s"],
                json!([
                    ["n.95percentile", 1, 5.0, null],
                    ["n.avg", 1, 5.0, null],
                    ["n.count", 0, 1.0, null],
                    ["n.max", 1, 5.0, null],
                    ["n.median", 1, 5.0, null],
                    ["u", 1, 1.0, null],
                ]),
            ),
        ];
        let options = ParseOptions {
            extended_metric_types: true,
        };
        for (format, lines, expected) in cases {
            let mut aggregator = Aggregator::new(EMPTY_TAGS, 10)
                .unwrap()
                .with_series_format(format);
            for line in &lines {
                let metric = parse_with_options(&format!("{line}|T1656581405"), options).unwrap();
                aggregator.insert(metric).unwrap();
            }
            // The bucket is still open one second before it ends.
            assert!(aggregator.consume_closed_metrics(1656581409).is_empty());

            let mut actual: Vec<_> = aggregator
                .consume_closed_metrics(1656581410)
                .iter()
                .flat_map(|batch| {
                    serde_json::to_value(batch).unwrap()["series"]
                        .as_array()
                        .unwrap()
                        .clone()
                })
                .map(|metric| {
                    json!([
                        metric["metric"],
                        metric["type"],
                        metric["points"][0]["value"],
                        metric.get("interval"),
                    ])
                })
                .collect();
            actual.sort_by_key(|metric| metric[0].as_str().unwrap().to_string());
            assert_eq!(json!(actual), expected, "lines {lines:?}");
        }
    }

    #[test]
    fn origin_override_table() {
        use crate::origin::{OriginCategory, OriginProduct, OriginService};

        // Metrics named datadog.* have no origin of their own.
        let cases = [(false, None), (true, Some(1))];
        for (with_origin, expected_product) in cases {
            let mut aggregator = Aggregator::new(EMPTY_TAGS, 10).unwrap();
            if with_origin {
                aggregator = aggregator.with_origin(
                    OriginProduct::Serverless,
                    OriginCategory::Other,
                    OriginService::Other,
                );
            }
            aggregator
                .insert(parse("datadog.level:7|g|T1656581405").unwrap())
                .unwrap();
            aggregator
                .insert(parse("datadog.latency:1|d|T1656581405").unwrap())
                .unwrap();

            let series = serde_json::to_value(&aggregator.consume_metrics()[0]).unwrap();
            let series_origin = &series["series"][0]["metadata"]["origin"];
            let sketches = aggregator.consume_distributions();
            let sketch_origin = sketches[0].sketches[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.origin.as_ref());

            match expected_product {
                None => {
                    assert!(series_origin.is_null(), "series origin {series_origin}");
                    assert!(sketch_origin.is_none(), "sketch origin {sketch_origin:?}");
                }
                Some(product) => {
                    assert_eq!(
                        *series_origin,
                        serde_json::json!({
                            "origin_product": product,
                            "origin_sub_product": 0,
                            "origin_product_detail": 0,
                        })
                    );
                    let sketch_origin = sketch_origin.unwrap();
                    assert_eq!(
                        (
                            sketch_origin.origin_product,
                            sketch_origin.origin_category,
                            sketch_origin.origin_service
                        ),
                        (product, 0, 0)
                    );
                }
            }
        }
    }

    #[test]
    fn runtime_origin_service_table() {
        use crate::origin::{OriginCategory, OriginProduct, OriginService};

        let cases = [
            (true, "runtime.python.cpu.time", 474),
            (true, "app.runtime.requests", 472),
            (false, "runtime.python.cpu.time", 472),
        ];
        for (with_runtime, name, expected_service) in cases {
            let mut aggregator = Aggregator::new(EMPTY_TAGS, 10).unwrap().with_origin(
                OriginProduct::Serverless,
                OriginCategory::ContainerAppMetrics,
                OriginService::ServerlessCustom,
            );
            if with_runtime {
                aggregator =
                    aggregator.with_runtime_origin_service(OriginService::ServerlessRuntime);
            }
            aggregator
                .insert(parse(&format!("{name}:7|g|T1656581405")).unwrap())
                .unwrap();

            let series = serde_json::to_value(&aggregator.consume_metrics()[0]).unwrap();
            assert_eq!(
                series["series"][0]["metadata"]["origin"],
                serde_json::json!({
                    "origin_product": 1,
                    "origin_sub_product": 37,
                    "origin_product_detail": expected_service,
                }),
                "{name}, with runtime {with_runtime}"
            );
        }
    }
}
