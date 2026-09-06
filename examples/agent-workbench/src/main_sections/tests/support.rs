use super::*;

pub(crate) fn in_memory_trigger_store() -> Arc<dyn lash::triggers::TriggerStore> {
    Arc::new(lash::triggers::InMemoryTriggerStore::new())
}

pub(crate) fn run_async_test_on_stack_budget<F, Fut>(name: &str, test: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(STACK_BUDGET_BYTES)
        .spawn(|| {
            let test = Box::pin(test());
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime")
                .block_on(test)
        })
        .expect("spawn stack-budget test thread")
        .join()
        .expect("stack-budget test thread");
}

pub(crate) fn run_async_test_on_stack_budget_multi_thread<F, Fut>(
    name: &str,
    worker_threads: usize,
    test: F,
) where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(STACK_BUDGET_BYTES)
        .spawn(move || {
            let test = Box::pin(test());
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(worker_threads)
                .thread_stack_size(STACK_BUDGET_BYTES)
                .enable_all()
                .build()
                .expect("tokio runtime")
                .block_on(test)
        })
        .expect("spawn stack-budget multi-thread test thread")
        .join()
        .expect("stack-budget multi-thread test thread");
}
