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
    /// Create a new `ApiKeyFactory` with a static API key.
    pub fn new(api_key: &str) -> Self {
        Self::Static(api_key.to_string())
    }

    /// Create a new `ApiKeyFactory` with a dynamic API key resolver function.
    pub fn new_from_resolver(resolver_fn: ApiKeyResolverFn) -> Self {
        Self::Dynamic {
            resolver_fn,
            api_key: Arc::new(OnceCell::new()),
        }
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
    async fn new() {
        let api_key_factory = ApiKeyFactory::new("mock-api-key");
        assert_eq!(api_key_factory.get_api_key().await, "mock-api-key");
    }
}
