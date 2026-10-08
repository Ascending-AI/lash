use super::*;

/// A backend with its catalog, trigger store or process work replaced, every
/// other port its own, for a test that decorates a store or observes the
/// process-event sink.
pub(crate) struct DecoratedBackend {
    layered: lash::testing::LayeredBackend,
}

impl DecoratedBackend {}

impl From<DecoratedBackend> for lash::Backend {
    fn from(decorated: DecoratedBackend) -> Self {
        decorated.layered.into_backend()
    }
}

pub(crate) fn run_async_test_on_stack_budget<F, Fut, T>(name: &str, test: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
    T: Send + 'static,
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
        .expect("stack-budget test thread")
}

/// A committed message's parts' content, one per line: how the laws read
/// what a message says.
pub(crate) fn message_text(message: &lash::messages::Message) -> String {
    message
        .parts
        .iter()
        .map(|part| part.content())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A committed message's role, by name.
pub(crate) fn message_role(message: &lash::messages::Message) -> &'static str {
    match message.role {
        lash::messages::MessageRole::User => "user",
        lash::messages::MessageRole::Assistant => "assistant",
        lash::messages::MessageRole::System => "system",
        lash::messages::MessageRole::Event => "event",
    }
}
