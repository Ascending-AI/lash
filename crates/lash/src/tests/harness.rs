use super::*;
use std::future::Future;

pub(super) const STACK_BUDGET_BYTES: usize = 2 * 1024 * 1024;

pub(super) fn model_spec(
    model: impl Into<String>,
    variant: Option<String>,
    context_window_tokens: usize,
) -> lash_core::ModelSpec {
    let capability = capability_for_variant(variant.as_deref());
    lash_core::ModelSpec::builder(model)
        .variant(
            variant
                .map(lash_core::ReasoningSelection::Effort)
                .unwrap_or_default(),
        )
        .context_window_tokens(context_window_tokens)
        .build()
        .expect("valid model spec")
        .with_capability(capability)
}

pub(super) fn mock_model_spec() -> lash_core::ModelSpec {
    model_spec("mock-model", None, 200_000)
}

pub(super) fn explicit_ephemeral_facets(
    builder: crate::core::LashCoreBuilder,
) -> crate::core::LashCoreBuilder {
    explicit_ephemeral_facets_with_budget(builder, crate::CommitBudget::bounded(1024 * 1024, 512))
}

pub(super) fn explicit_ephemeral_facets_without_session_store(
    builder: crate::core::LashCoreBuilder,
) -> crate::core::LashCoreBuilder {
    builder
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .effect_host(Arc::new(
            crate::durability::NativeEffectHost::default().allow_process_lifetime_completion_keys(),
        ))
        .attachment_store(Arc::new(crate::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            crate::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .without_queued_work()
}

pub(super) fn core_without_session_store() -> LashCore {
    explicit_ephemeral_facets_without_session_store(LashCore::standard_builder(
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())
    .expect("complete non-storage fixture")
}

pub(super) fn explicit_ephemeral_facets_with_budget(
    builder: crate::core::LashCoreBuilder,
    commit_budget: crate::CommitBudget,
) -> crate::core::LashCoreBuilder {
    builder
        .commit_budget(commit_budget)
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .effect_host(Arc::new(
            crate::durability::NativeEffectHost::default().allow_process_lifetime_completion_keys(),
        ))
        .attachment_store(Arc::new(crate::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            crate::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .store_factory(Arc::new(
            crate::persistence::InMemorySessionStoreFactory::new(),
        ))
        .without_queued_work()
}

fn capability_for_variant(variant: Option<&str>) -> lash_core::ModelCapability {
    let Some(variant) = variant else {
        return lash_core::ModelCapability::default();
    };
    lash_core::ModelCapability {
        instruction_role: Default::default(),
        native_mid_conversation_system: false,
        attachment_acceptance: Default::default(),
        google_dialect: Default::default(),
        reasoning: Some(lash_core::ReasoningCapability {
            efforts: vec![variant.to_string()],
            default_effort: None,
            aliases: Default::default(),
            encoding: lash_core::ReasoningEncoding::Effort,
            disable: None,
            mandatory: false,
        }),
        cache_control: None,
        stream_termination: None,
        sampling: lash_core::SamplingCapability::Configurable,
    }
}

pub(super) fn run_async_test_on_stack_budget<F, Fut, T>(name: &str, test: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
    T: Send + 'static,
{
    run_async_test_on_stack_size(name, STACK_BUDGET_BYTES, test)
}

pub(super) fn run_async_test_on_stack_size<F, Fut, T>(name: &str, stack_size: usize, test: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(stack_size)
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
