use std::fmt::Debug;
use std::sync::Arc;
use std::{future::Future, pin::Pin};
use tokio::sync::OnceCell;

pub type ApiKeyResolverFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = String> + Send>> + Send + Sync>;

#[derive(Clone)]
pub enum ApiKeyFactory {
    Static(String),
    Dynamic {
        resolver_fn: ApiKeyResolverFn,
        api_key: Arc<OnceCell<String>>,
    },
}

impl ApiKeyFactory {
    pub fn new_from_resolver(resolver_fn: ApiKeyResolverFn) -> Self {
        Self::Dynamic {
            resolver_fn,
            api_key: Arc::new(OnceCell::new()),
        }
    }

    pub fn new_from_static_key(api_key: &str) -> Self {
        Self::Static(api_key.to_string())
    }

    pub async fn get_api_key(&self) -> &str {
        match self {
            Self::Static(api_key) => api_key,
            Self::Dynamic {
                resolver_fn,
                api_key,
            } => {
                api_key
                    .get_or_init(|| async { (resolver_fn)().await })
                    .await
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
    async fn new_from_resolver() {
        let api_key_factory = Arc::new(ApiKeyFactory::new_from_resolver(Arc::new(move || {
            let api_key = "mock-api-key".to_string();
            Box::pin(async move { api_key })
        })));
        assert_eq!(api_key_factory.get_api_key().await, "mock-api-key");
    }

    #[tokio::test]
    async fn new_from_static_key() {
        let api_key_factory = ApiKeyFactory::new_from_static_key("mock-api-key");
        assert_eq!(api_key_factory.get_api_key().await, "mock-api-key");
    }
}
