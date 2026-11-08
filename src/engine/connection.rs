//! Single-flight initialization without serializing operations. Failed
//! operations invalidate only their own generation; no business call is retried.

use std::{ffi::OsString, future::Future, sync::Arc};

use tokio::sync::Mutex;

struct State<T> {
    endpoint: Option<OsString>,
    client: Option<Arc<T>>,
}

pub(super) struct ClientCache<T> {
    state: Mutex<State<T>>,
}

impl<T> Default for ClientCache<T> {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                endpoint: None,
                client: None,
            }),
        }
    }
}

impl<T> ClientCache<T> {
    pub(super) async fn get<E, F>(
        &self,
        endpoint: Option<OsString>,
        initialize: impl FnOnce() -> F,
    ) -> Result<Arc<T>, E>
    where
        F: Future<Output = Result<T, E>>,
    {
        let _timing = crate::diagnostics::PerfSpan::new("engine.client.acquire");
        let mut state = self.state.lock().await;
        if state.endpoint == endpoint {
            if let Some(client) = &state.client {
                return Ok(client.clone());
            }
        }
        // Drop the old pool even if connecting to the new endpoint fails.
        state.client = None;
        state.endpoint = endpoint;
        let client = Arc::new(initialize().await?);
        state.client = Some(client.clone());
        Ok(client)
    }

    pub(super) async fn invalidate(&self, failed: &Arc<T>) {
        let mut state = self.state.lock().await;
        if state
            .client
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, failed))
        {
            state.client = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn concurrent_acquisition_initializes_once() {
        let cache = ClientCache::default();
        let count = AtomicUsize::new(0);
        let initialize = || async {
            count.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            Ok::<_, ()>(42)
        };
        let (a, b) = tokio::join!(cache.get(None, initialize), cache.get(None, initialize));
        assert!(Arc::ptr_eq(&a.unwrap(), &b.unwrap()));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_initialization_is_not_cached() {
        let cache = ClientCache::default();
        assert!(cache
            .get(None, || async { Err::<u8, _>(()) })
            .await
            .is_err());
        assert_eq!(
            *cache.get(None, || async { Ok::<_, ()>(7) }).await.unwrap(),
            7
        );
    }

    #[tokio::test]
    async fn late_failure_does_not_invalidate_replacement() {
        let cache = ClientCache::default();
        let old = cache.get(None, || async { Ok::<_, ()>(1) }).await.unwrap();
        cache.invalidate(&old).await;
        let new = cache.get(None, || async { Ok::<_, ()>(2) }).await.unwrap();
        cache.invalidate(&old).await;
        let current = cache.get(None, || async { Ok::<_, ()>(3) }).await.unwrap();
        assert!(Arc::ptr_eq(&new, &current));
    }

    #[tokio::test]
    async fn acquisition_releases_lock_before_the_operation_runs() {
        let cache = ClientCache::default();
        let first = cache.get(None, || async { Ok::<_, ()>(42) }).await.unwrap();
        // Keeping an operation's Arc alive must not hold the initialization
        // mutex, even for persistent exec/log streams.
        let second = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            cache.get(None, || async { Ok::<_, ()>(99) }),
        )
        .await
        .expect("active operation must not serialize other acquisitions")
        .unwrap();
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[tokio::test]
    async fn invalidation_does_not_retry_an_operation_or_connect_early() {
        let cache = ClientCache::default();
        let count = AtomicUsize::new(0);
        let initialize = || async {
            count.fetch_add(1, Ordering::SeqCst);
            Ok::<_, ()>(42)
        };
        let first = cache.get(None, initialize).await.unwrap();
        cache.invalidate(&first).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
        cache.get(None, initialize).await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn changed_endpoint_uses_a_new_pool() {
        let cache = ClientCache::default();
        let old = cache.get(None, || async { Ok::<_, ()>(1) }).await.unwrap();
        let new = cache
            .get(Some("npipe://other".into()), || async { Ok::<_, ()>(2) })
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&old, &new));
        assert_eq!(*new, 2);
    }
}
