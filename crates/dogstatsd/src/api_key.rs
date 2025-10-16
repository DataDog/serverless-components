use std::fmt::Debug;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use std::{future::Future, pin::Pin};
use tokio::sync::RwLock;
use tracing::debug;

pub type ApiKeyResolverFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Option<String>> + Send>> + Send + Sync>;

#[derive(Default)]
pub struct ApiKeyState {
    api_key: Option<String>,
    last_load_time: Option<Instant>,
}

pub enum ApiKeyFactory {
    Static(String),
    Dynamic {
        resolver_fn: ApiKeyResolverFn,
        // How often to reload the api key. If None, the api key will only be loaded once.
        // Reload checks only happen on reads of the api key.
        reload_interval: Option<Duration>,
        api_key_state: Arc<RwLock<ApiKeyState>>,
        // Whether the api key is currently being loaded. A lock to avoid concurrent loads.
        loading_api_key: AtomicBool,
    },
}

impl ApiKeyFactory {
    /// Create a new `ApiKeyFactory` with a static API key.
    pub fn new(api_key: &str) -> Self {
        Self::Static(api_key.to_string())
    }

    /// Create a new `ApiKeyFactory` with a dynamic API key resolver function.
    pub fn new_from_resolver(
        resolver_fn: ApiKeyResolverFn,
        reload_interval: Option<Duration>,
    ) -> Self {
        if let Some(reload_interval) = reload_interval {
            debug!(
                "Creating ApiKeyFactory with reload interval: {:?}",
                reload_interval
            );
        }
        Self::Dynamic {
            resolver_fn,
            reload_interval,
            api_key_state: Arc::new(RwLock::new(ApiKeyState::default())),
            loading_api_key: AtomicBool::new(false),
        }
    }

    pub async fn get_api_key(&self) -> Option<String> {
        match self {
            Self::Static(api_key) => Some(api_key.clone()),
            Self::Dynamic {
                resolver_fn,
                api_key_state,
                loading_api_key,
                ..
            } => {
                if self.should_load_api_key().await {
                    // Try to acquire the loading lock.
                    if (loading_api_key
                        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                        .is_ok())
                    {
                        // Acquired the loading lock.
                        // Double-check: verify load is still needed after acquiring lock
                        // This prevents duplicate loads from multiple threads
                        if self.should_load_api_key().await {
                            let api_key_value = (resolver_fn)().await;
                            *api_key_state.write().await = ApiKeyState {
                                api_key: api_key_value.clone(),
                                last_load_time: Some(Instant::now()),
                            };
                        }

                        loading_api_key.store(false, Ordering::Release);
                    } else {
                        // Failed to acquire the loading lock, which means another thread is doing the load.
                        // If there is an old api key, break out and return it.
                        // (We assume the old api key will still be valid for a while.)
                        // If there is no old api key, wait for another thread to complete the initial load.
                        // We check last_load_time instead of api_key because if we check api_key and
                        // the resolver function returns None, this thread would wait forever.
                        while api_key_state.read().await.last_load_time.is_none() {
                            tokio::task::yield_now().await;
                        }
                    }
                }

                api_key_state.read().await.api_key.clone()
            }
        }
    }

    async fn should_load_api_key(&self) -> bool {
        match self {
            Self::Static(_) => false,
            Self::Dynamic {
                reload_interval,
                api_key_state,
                ..
            } => {
                match api_key_state.read().await.last_load_time {
                    // Initial load
                    None => true,
                    // Not initial load
                    Some(last_load_time) => {
                        match *reload_interval {
                            // User's configuration says do not reload
                            None => false,
                            // Reload only if it has been longer than reload interval since last load
                            Some(reload_interval) => {
                                Instant::now() > last_load_time + reload_interval
                            }
                        }
                    }
                }
            }
        }
    }
}

impl Debug for ApiKeyFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ApiKeyFactory")
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[tokio::test]
    async fn test_new() {
        let api_key_factory = ApiKeyFactory::new("mock-api-key");
        assert_eq!(
            api_key_factory.get_api_key().await,
            Some("mock-api-key".to_string())
        );
    }

    #[tokio::test]
    async fn test_resolver_no_reload() {
        let api_key_factory = Arc::new(ApiKeyFactory::new_from_resolver(
            Arc::new(move || {
                let api_key = "mock-api-key".to_string();
                Box::pin(async move { Some(api_key) })
            }),
            None,
        ));
        assert_eq!(
            api_key_factory.get_api_key().await,
            Some("mock-api-key".to_string()),
        );
    }

    #[tokio::test]
    async fn test_resolver_with_reload() {
        let counter = Arc::new(RwLock::new(0));
        let counter_clone = counter.clone();

        // Return different api keys on each call
        let api_key_factory = Arc::new(ApiKeyFactory::new_from_resolver(
            Arc::new(move || {
                let counter = counter_clone.clone();
                Box::pin(async move {
                    let mut count = counter.write().await;
                    *count += 1;
                    Some(format!("mock-api-key-{}", *count))
                })
            }),
            Some(Duration::from_millis(1)),
        ));

        // First call - should return "mock-api-key-1"
        let first_key = api_key_factory.get_api_key().await;
        assert_eq!(first_key, Some("mock-api-key-1".to_string()));

        // Sleep for 1 millisecond to allow reload
        tokio::time::sleep(Duration::from_millis(1)).await;

        // Second call - should return "mock-api-key-2" (after reload)
        let second_key = api_key_factory.get_api_key().await;
        assert_eq!(second_key, Some("mock-api-key-2".to_string()));
    }
}
