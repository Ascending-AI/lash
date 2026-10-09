//! Durable capacity routing and queue admission, before pool checkout.
use super::*;
use tokio::sync::OwnedSemaphorePermit;

impl PostgresDurableStore {
    pub(super) fn route(&self, capacity: CommitCapacity) -> Route<'_> {
        let pools = &*self.pools;
        let preludes = &pools.preludes;
        match capacity {
            CommitCapacity::Renewal => Route {
                pool: &pools.renewal,
                prelude: &preludes.renewal,
                admitted: false,
            },
            CommitCapacity::Scheduler => Route {
                pool: &pools.scheduler,
                prelude: &preludes.scheduler,
                admitted: false,
            },
            CommitCapacity::Critical => Route {
                pool: &pools.critical,
                prelude: &preludes.durable,
                admitted: false,
            },
            CommitCapacity::Work => Route {
                pool: &pools.work,
                prelude: &preludes.durable,
                admitted: true,
            },
        }
    }

    /// Run `operation` on `capacity` within its deadline, admitted first
    /// when it runs on the work pool. The permit is the operation's: it is
    /// released when the operation ends, never held past it.
    pub(super) async fn within<T>(
        &self,
        capacity: CommitCapacity,
        operation: impl std::future::Future<Output = Result<T, DurableError>>,
    ) -> Result<T, DurableError> {
        let route = self.route(capacity);
        // Boxed: every store operation runs through here, and its body is
        // the largest future of the call.
        let operation = Box::pin(operation);
        route
            .prelude
            .bounded(async {
                #[cfg(feature = "perf-witness")]
                let mut timing = if route.admitted {
                    lash_core_execution::perf_witness::queues::Timer::enqueue(
                        "postgres.work_admission",
                    )
                } else {
                    None
                };
                let _admission = self.admit(&route).await?;
                #[cfg(feature = "perf-witness")]
                if let Some(timing) = &mut timing {
                    timing.start_service();
                }
                let result = operation.await;
                #[cfg(feature = "perf-witness")]
                if let Some(timing) = &mut timing {
                    timing.complete();
                }
                result
            })
            .await
            .unwrap_or_else(|deadline| Err(deadline_failure(deadline)))
    }

    async fn admit(&self, route: &Route<'_>) -> Result<Option<OwnedSemaphorePermit>, DurableError> {
        if !route.admitted {
            return Ok(None);
        }
        Arc::clone(&self.pools.admission)
            .acquire_owned()
            .await
            .map(Some)
            .map_err(|_| {
                DurableError::Store(StoreFailure {
                    kind: StoreFailureKind::Unavailable,
                    message: "the durable store's admission is closed".to_owned(),
                })
            })
    }
}

#[cfg(all(test, feature = "perf-witness"))]
mod tests {
    use super::*;
    use lash_core_execution::perf_witness::queues::Collector;
    use std::future::Future;
    use std::time::{Duration, Instant};

    // The physical admission rule excludes semaphore backlog from service.
    #[tokio::test]
    async fn admission_backlog_is_queue_wait_not_service() {
        let url = crate::postgres_test_support::database_url().expect("hermetic PostgreSQL URL");
        let database = crate::testing::IsolatedDatabase::create(&url).await;
        let storage = crate::testing::connect(database.url())
            .await
            .expect("open isolated store");
        let store = storage.durable_store();
        let permits = store.pools.admission.available_permits() as u32;
        let held = Arc::clone(&store.pools.admission)
            .acquire_many_owned(permits)
            .await
            .expect("hold admission");
        let collector =
            Collector::install_for_thread(std::thread::current().id()).expect("queue collector");
        let mut waiting = Box::pin(store.now());
        std::future::poll_fn(|cx| {
            assert!(waiting.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let blocked = Instant::now();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let forced_wait = blocked.elapsed();
        drop(held);
        waiting.await.expect("operation admitted");
        let snapshot = collector.snapshot();
        let record = snapshot
            .records
            .iter()
            .find(|r| r.site == "postgres.work_admission")
            .expect("admission receipt");
        assert!(u128::from(record.queue_wait_ns) >= forced_wait.as_nanos());
        assert_eq!(record.elapsed_ns, record.queue_wait_ns + record.service_ns);
        assert!(record.completed);
        drop(collector);
        drop(store);
        drop(storage);
        drop(database);
    }
}
