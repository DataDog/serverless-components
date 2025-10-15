use bytes::Bytes;
use reqwest::header::HeaderMap;

#[derive(Clone)]
pub struct ProxyRequest {
    pub headers: HeaderMap,
    pub body: Bytes,
    pub target_url: String,
}

/// Takes in individual proxy requests and aggregates them into batches to be flushed to Datadog.
pub struct ProxyAggregator {
    queue: Vec<ProxyRequest>,
}

impl Default for ProxyAggregator {
    fn default() -> Self {
        ProxyAggregator {
            queue: Vec::with_capacity(128), // arbitrary capacity for request queue
        }
    }
}

impl ProxyAggregator {
    /// Takes in an individual proxy request.
    pub fn add(&mut self, request: ProxyRequest) {
        self.queue.push(request);
    }

    /// Returns a batch of proxy requests.
    pub fn get_batch(&mut self) -> Vec<ProxyRequest> {
        std::mem::take(&mut self.queue)
    }

    /// Flush the queue.
    pub fn clear(&mut self) {
        self.queue.clear();
    }
}