// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use crate::aggregator::Aggregator;
use crate::datadog::Series;
use crate::errors;
use crate::metric::{Metric, SortedTags};
use datadog_protos::metrics::SketchPayload;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub struct DoubleBufferedAggregator {
    buffers: [Arc<Mutex<Aggregator>>; 2],
    active_index: AtomicUsize,
}

impl DoubleBufferedAggregator {
    pub fn new(tags: SortedTags, max_context: usize) -> Result<Self, errors::Creation> {
        Ok(Self {
            buffers: [
                Arc::new(Mutex::new(Aggregator::new(tags.clone(), max_context)?)),
                Arc::new(Mutex::new(Aggregator::new(tags, max_context)?)),
            ],
            active_index: AtomicUsize::new(0),
        })
    }

    pub fn insert(&self, metric: Metric) -> Result<(), errors::Insert> {
        let index = self.active_index.load(Ordering::Acquire);
        let buffer = &self.buffers[index];
        
        #[allow(clippy::expect_used)]
        let mut aggregator = buffer.lock().expect("lock poisoned");
        aggregator.insert(metric)
    }

    pub fn flush(&self) -> (Vec<Series>, Vec<SketchPayload>) {
        let old_index = self.active_index.load(Ordering::Acquire);
        let new_index = 1 - old_index;
        
        self.active_index.store(new_index, Ordering::Release);
        
        std::thread::yield_now();
        
        let flush_buffer = &self.buffers[old_index];
        
        #[allow(clippy::expect_used)]
        let mut aggregator = flush_buffer.lock().expect("lock poisoned");
        
        let series = aggregator.consume_metrics();
        let distributions = aggregator.consume_distributions();
        
        (series, distributions)
    }

    #[cfg(test)]
    pub fn get_active_aggregator(&self) -> Arc<Mutex<Aggregator>> {
        let index = self.active_index.load(Ordering::Acquire);
        Arc::clone(&self.buffers[index])
    }

    #[cfg(test)]
    pub fn get_inactive_aggregator(&self) -> Arc<Mutex<Aggregator>> {
        let index = self.active_index.load(Ordering::Acquire);
        Arc::clone(&self.buffers[1 - index])
    }
    
    #[cfg(test)]
    pub fn peek_metrics(&self) -> (crate::datadog::Series, datadog_protos::metrics::SketchPayload) {
        let index = self.active_index.load(Ordering::Acquire);
        let buffer = &self.buffers[index];
        
        #[allow(clippy::expect_used)]
        let aggregator = buffer.lock().expect("lock poisoned");
        
        (aggregator.to_series(), aggregator.distributions_to_protobuf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metric::{parse, EMPTY_TAGS};

    #[test]
    fn test_double_buffer_switching() {
        let aggregator = DoubleBufferedAggregator::new(EMPTY_TAGS, 100).unwrap();
        
        let metric1 = parse("test1:1|c|#k:v").expect("metric parse failed");
        assert!(aggregator.insert(metric1).is_ok());
        
        {
            let active = aggregator.get_active_aggregator();
            let active_guard = active.lock().unwrap();
            assert_eq!(active_guard.to_series().len(), 1);
        }
        
        let (series, _) = aggregator.flush();
        assert_eq!(series[0].series.len(), 1);
        
        {
            let inactive = aggregator.get_inactive_aggregator();
            let inactive_guard = inactive.lock().unwrap();
            assert_eq!(inactive_guard.to_series().len(), 0);
        }
        
        let metric2 = parse("test2:2|c|#k:v").expect("metric parse failed");
        assert!(aggregator.insert(metric2).is_ok());
        
        {
            let active = aggregator.get_active_aggregator();
            let active_guard = active.lock().unwrap();
            assert_eq!(active_guard.to_series().len(), 1);
        }
    }

    #[test]
    fn test_concurrent_operations() {
        use std::thread;
        use std::time::Duration;
        
        let aggregator = Arc::new(DoubleBufferedAggregator::new(EMPTY_TAGS, 1000).unwrap());
        
        let aggregator_insert = Arc::clone(&aggregator);
        let insert_handle = thread::spawn(move || {
            for i in 0..100 {
                let metric = parse(&format!("test{}:{}|c", i, i)).expect("metric parse failed");
                aggregator_insert.insert(metric).expect("insert failed");
                thread::sleep(Duration::from_micros(10));
            }
        });
        
        let aggregator_flush = Arc::clone(&aggregator);
        let flush_handle = thread::spawn(move || {
            let mut total_flushed = 0;
            for _ in 0..5 {
                thread::sleep(Duration::from_millis(5));
                let (series, _) = aggregator_flush.flush();
                for batch in series {
                    total_flushed += batch.series.len();
                }
            }
            total_flushed
        });
        
        insert_handle.join().unwrap();
        let total_flushed = flush_handle.join().unwrap();
        
        let (final_series, _) = aggregator.flush();
        let final_count: usize = final_series.iter().map(|s| s.series.len()).sum();
        
        assert_eq!(total_flushed + final_count, 100);
    }
}