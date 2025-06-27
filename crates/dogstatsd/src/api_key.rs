use std::{future::Future, pin::Pin};
use std::sync::Arc;
use tokio::sync::OnceCell;
use std::fmt::Debug;

pub type ApiKeyFactoryFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = String> + Send>> + Send + Sync>;

enum ApiKeyFactoryInner {
    Static(String),
    Dynamic {
        resolve_key_fn: ApiKeyFactoryFn,
        api_key: OnceCell<String>,
    },
}

#[derive(Clone)]
pub struct ApiKeyFactory {
    inner: Arc<ApiKeyFactoryInner>
}

impl ApiKeyFactory {
    pub fn new_with_fn(resolve_key_fn: ApiKeyFactoryFn) -> Self {
        Self {
            inner: Arc::new(ApiKeyFactoryInner::Dynamic {
                resolve_key_fn,
                api_key: OnceCell::new(),
            }),
        }
    }

    pub fn new_with_api_key(api_key: &str) -> Self {
        Self {
            inner: Arc::new(ApiKeyFactoryInner::Static(api_key.to_string())),
        }
    }

    pub async fn get_api_key(&self) -> &str {
        match self.inner.as_ref() {
            ApiKeyFactoryInner::Static(api_key) => api_key,
            ApiKeyFactoryInner::Dynamic { resolve_key_fn, api_key } => {
                api_key.get_or_init(|| async { (resolve_key_fn)().await }).await
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
    use crate::api_key::ApiKeyFactory;
    use std::sync::Arc;

    #[tokio::test]
    async fn new_with_fn() {
        let api_key_factory = Arc::new(ApiKeyFactory::new_with_fn(Arc::new(move || {
            let api_key = "mock-api-key".to_string();
            Box::pin(async move { api_key })
        })));
        assert_eq!(api_key_factory.get_api_key().await, "mock-api-key");
    }

    #[tokio::test]
    async fn new_with_api_key() {
        let api_key_factory = ApiKeyFactory::new_with_api_key("mock-api-key");
        assert_eq!(api_key_factory.get_api_key().await, "mock-api-key");
    }
}
