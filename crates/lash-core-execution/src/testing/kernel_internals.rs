//! Crate-internal kernel surface the relocated store-backed tests reach for.
//!
//! Those tests were unit tests inside this crate until FIG-3582 moved every
//! test that needs a concrete store or effect host to
//! `tests/store_backed`, over a SQLite memory store set (ADR 0102): the store
//! depends on this crate, so an in-crate `cfg(test)` module could not use it.
//! This module is the `testing`-feature seam that keeps exactly the surface
//! they reach reachable without widening the crate's shipped API. A
//! crate-private function is reached through a wrapper here; a type or
//! function already declared `pub` inside a private module is re-exported.

// The crate-root short paths (`crate::X`) the relocated tests used in-crate
// that the root re-exports only to the crate. Each item is already public at
// its own path; these re-exports keep the short path the tests were written
// against.
pub use crate::plugin::{
    RuntimeServices, SessionObservedProcessOutcome, SessionObservedProcessReceipt,
    SessionObserverIntent,
};

pub use crate::runtime::load_process_env;
pub use crate::session::{RuntimeExecutionTracing, Session};

use crate::runtime::process::{ObservedWorkItem, ProcessRecord, ProcessWorkObserver};
use crate::{PluginError, ProcessId};

/// `ProcessWorkObserver::work_item_from_record`: the bounded record/event-tail
/// retry the observation laws shift with a record read before a concurrent
/// write.
pub async fn work_item_from_record(
    observer: &ProcessWorkObserver,
    record: ProcessRecord,
) -> Result<ObservedWorkItem, PluginError> {
    observer.work_item_from_record(record).await
}

/// `process_terminal_resolution`: the resolution a process terminal delivers
/// to the waits armed on it, which the attach-terminal laws compare a parked
/// wait against.
pub fn process_terminal_resolution(output: crate::ProcessAwaitOutput) -> crate::Resolution {
    crate::runtime::effect::executor::process_terminal_resolution(output)
}

/// A read-only view of `plugin_id`'s namespace over a session's published
/// plugin state: the view a plugin receives.
pub fn plugin_state_view(
    plugins: &crate::PluginSession,
    plugin_id: &str,
) -> crate::PluginStateView {
    plugins.plugin_state_view_for_testing(plugin_id)
}

/// Publish `commands` to `plugin_id`'s namespace the way a body of that
/// plugin's tool does: reduced inside a recorded body at `address`, then
/// published once the body's outcome returns. The returned outcome is what a
/// journal retains for the body; publishing it again applies nothing.
pub async fn publish_plugin_state(
    plugins: &std::sync::Arc<crate::PluginSession>,
    plugin_id: &str,
    address: crate::EffectAddress,
    commands: crate::StateCommands,
) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
    let owner = plugins
        .host()
        .factories()
        .iter()
        .find(|factory| factory.id() == plugin_id)
        .ok_or_else(|| {
            crate::PluginError::Registration(format!("plugin `{plugin_id}` is not installed"))
        })?;
    let proposal = crate::plugin::Proposal::for_tool(
        owner.id().to_owned(),
        crate::plugin::StateCommandOrigin::ToolAttempt {
            call_id: crate::ToolCallId::fixture(&address.replay_key),
            attempt: lash_core_store::tool_run::AttemptOrdinal::FIRST,
        },
        commands,
    );
    let body_plugins = std::sync::Arc::clone(plugins);
    let recorded = crate::plugin::state::record_effect(
        plugins.clone(),
        crate::RuntimeEffectKind::LanguageRuntimeValue,
        address.clone(),
        async move {
            crate::plugin::propose(body_plugins.as_ref(), proposal)
                .map_err(crate::PluginError::from)?;
            Ok(crate::RuntimeEffectOutcome::LanguageRuntimeValue {
                value: serde_json::Value::Null,
            })
        },
    )
    .await?;
    crate::plugin::EffectPublication::begin(plugins.clone(), address).publish(recorded.clone())?;
    Ok(recorded)
}

/// `RuntimeExecutionTracing::emit_tool_call_completed`: the trace a completed
/// tool call emits, which the retry-trace law reads back. The law calls it
/// outside any journal, so the call is its own live attempt.
pub fn emit_tool_call_completed(
    tracing: &crate::session::RuntimeExecutionTracing,
    standing: &crate::trace::TraceStanding,
    record: &crate::ToolCallRecord,
    attempts: &[lash_trace::TraceRetryAttempt],
    issuing_node_id: Option<&str>,
    duration_ms: u64,
) {
    let _ = tracing;
    let content = standing.content();
    standing.observe(|| {
        (
            lash_trace::TraceContext::default(),
            lash_trace::TraceEvent::ToolCallCompleted {
                call_id: record.call_id.clone(),
                provider_call_id: record.provider_call_id.clone(),
                name: record.tool.clone(),
                args: content.capture(|| record.args.clone()),
                output: crate::trace::trace_tool_call_output(&record.output, content),
                duration_ms,
                issuing_node_id: issuing_node_id.map(str::to_string),
                attempts: (!attempts.is_empty()).then(|| attempts.to_vec()),
            },
        )
    });
}

// `RuntimeExecutionContext`'s process-handle and process-await operations:
// what a language runtime's process host calls on a context, which the
// relocated handle, await and batch laws shift directly.

pub async fn await_process_handle(
    context: &crate::RuntimeExecutionContext<'_>,
    call_id: crate::ToolCallId,
    handle: serde_json::Value,
) -> crate::session::ToolInvocationReply {
    context.await_process_handle(call_id, handle).await
}

pub async fn cancel_process_handle(
    context: &crate::RuntimeExecutionContext<'_>,
    call_id: crate::ToolCallId,
    handle: serde_json::Value,
) -> crate::session::ToolInvocationReply {
    context.cancel_process_handle(call_id, handle).await
}

pub fn record_started_process(
    context: &crate::RuntimeExecutionContext<'_>,
    process_id: &ProcessId,
) {
    context.record_started_process(process_id);
}

pub async fn await_process_with_cancellation(
    context: &crate::RuntimeExecutionContext<'_>,
    process_id: &crate::ProcessId,
    parent_invocation: Option<crate::RuntimeInvocation>,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Result<crate::ProcessAwaitOutput, PluginError> {
    context
        .await_process_with_cancellation(process_id, parent_invocation, cancellation)
        .await
}

/// `dispatch_tool_call_with_execution_context`: dispatches one call by name
/// under the dispatch state `call` configures.
pub async fn dispatch_tool_call_with_execution_context<'run>(
    context: &crate::tool_dispatch::ToolDispatchContext<'run>,
    tool_name: String,
    args: serde_json::Value,
    call: super::ToolCallFixture<'run>,
) -> crate::tool_dispatch::ToolDispatchOutcome {
    Box::pin(
        crate::tool_dispatch::dispatch_tool_call_with_execution_context(
            context,
            tool_name,
            args,
            call.context,
        ),
    )
    .await
}

/// `coordinate_prepared_tool_call_launch_with_execution_context`: coordinates
/// a prepared call's attempts under the dispatch state `call` configures.
pub async fn coordinate_prepared_tool_call_launch_with_execution_context<'run>(
    context: &crate::tool_dispatch::ToolDispatchContext<'run>,
    prepared: crate::PreparedToolCall,
    execution_grant: Option<Box<crate::ToolExecutionGrant>>,
    call: super::ToolCallFixture<'run>,
) -> crate::tool_dispatch::ToolCallLaunch {
    Box::pin(
        crate::tool_dispatch::coordinate_prepared_tool_call_launch_with_execution_context(
            context,
            prepared,
            execution_grant,
            call.context,
        ),
    )
    .await
}

/// `execute_once`: runs a leaf tool body exactly once, with no retry ladder.
pub async fn execute_once<'run>(
    context: &crate::tool_dispatch::ToolDispatchContext<'run>,
    prepared: &crate::PreparedToolCall,
    call: super::ToolCallFixture<'run>,
    grant: Option<&crate::ToolExecutionGrant>,
) -> crate::ToolAttemptOutcome {
    Box::pin(crate::tool_dispatch::execute_once(
        context,
        prepared,
        call.context,
        grant,
    ))
    .await
}

/// `RuntimeEffectLocalExecutor::prepared_tool_attempt`: the local executor of
/// one attempt of a prepared call, under the dispatch state `call` configures.
pub fn prepared_tool_attempt<'run>(
    dispatch: std::sync::Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
    call: super::ToolCallFixture<'run>,
) -> crate::RuntimeEffectLocalExecutor<'run> {
    crate::RuntimeEffectLocalExecutor::prepared_tool_attempt(dispatch, call.context)
}

/// Complete a settled output through the journaled presentation boundary.
pub async fn complete_tool_output(
    context: &crate::RuntimeExecutionContext<'_>,
    call_id: crate::ToolCallId,
    tool: &str,
    output: crate::ToolCallOutput,
) -> Result<crate::ModelToolReturn, crate::RuntimeEffectControllerError> {
    context
        .complete_tool_call(
            crate::tool_dispatch::ToolCallIds {
                call_id: call_id.clone(),
                provider_call_id: None,
            },
            crate::ToolId::new(tool),
            None,
            crate::tool_dispatch::ToolDispatchOutcome {
                record: crate::ToolCallRecord {
                    call_id,
                    provider_call_id: None,
                    tool: tool.to_string(),
                    args: serde_json::json!({}),
                    output,
                },
                attempts: Vec::new(),
                intents: crate::ToolIntents::default(),
                intent_outcomes: Vec::new(),
            },
            "test:call",
            1,
        )
        .await
        .map(|result| result.completed.model_return)
}

/// One plugin's prompt registrations, as its `register` makes them.
pub type PromptRegistration =
    Box<dyn Fn(&mut crate::plugin::PluginRegistrar) -> Result<(), PluginError>>;

/// The prompt catalog of `plugins`, each registered under its id at behavior
/// revision one, in order, as a session build registers them.
pub fn prompt_catalog(
    plugins: Vec<(&'static str, PromptRegistration)>,
) -> Result<crate::plugin::prompt::PromptCatalog, PluginError> {
    let mut contributions = crate::plugin::PluginContributions::default();
    let mut entries = Vec::new();
    for (id, register) in plugins {
        let mut recipe = crate::plugin::PluginRegistrar::new(id);
        register(&mut recipe)?;
        entries.extend(recipe.descriptors(id, crate::plugin::RegistryKind::Session));
        let owner = crate::RuntimeOwner::Session("prompt-fixture".into());
        let state = crate::plugin::PluginStateView::bind(&owner, id, std::sync::Arc::default());
        let mut reg = crate::plugin::BoundRegistrar::new(id.into());
        reg.contributions = contributions;
        recipe.bind(&(), &state, &mut reg)?;
        contributions = reg.contributions;
    }
    Ok(crate::plugin::prompt::PromptCatalog::new(
        contributions.prompt,
        crate::plugin::prompt::PromptCatalogSnapshot::from_inventory(
            &crate::plugin::InstalledInventory {
                scope: crate::plugin::InventoryScope::Deployment(
                    crate::plugin::RegistryKind::Session,
                ),
                readiness: vec![crate::plugin::ContributionReadiness::NotValidated; entries.len()],
                entries,
            },
        ),
    ))
}

/// Explicitly binds prompt fixture recipes. This is an executable preview,
/// never a diagnostics read or an installed owner observation.
pub fn prompt_catalog_from_recipes(
    recipes: &[crate::plugin::PluginRecipe],
) -> Result<crate::plugin::prompt::PromptCatalog, PluginError> {
    let owner = crate::RuntimeOwner::Session("prompt-recipe-fixture".into());
    let context = crate::plugin::PluginSessionContext {
        bindings: std::sync::Arc::new(crate::plugin::ServiceBindings::default()),
        owner: owner.clone(),
        tracing: crate::trace::TraceRuntime::new(std::sync::Arc::new(crate::SystemClock)),
        trace: None,
        plugin_config: crate::plugin::AdmittedPluginConfig::default(),
        materialization: crate::plugin::PluginSessionMaterialization::Creation,
        extensions: crate::plugin::PluginExtensions::default(),
    };
    let mut contributions = crate::plugin::PluginContributions::default();
    let entries = recipes
        .iter()
        .flat_map(|recipe| recipe.entries.clone())
        .collect::<Vec<_>>();
    for recipe in recipes {
        let id = recipe
            .entries
            .first()
            .map(|entry| entry.key.owner())
            .unwrap_or("prompt-fixture");
        let state = crate::plugin::PluginStateView::bind(&owner, id, std::sync::Arc::default());
        let mut reg = crate::plugin::BoundRegistrar::new(id.into());
        reg.contributions = contributions;
        recipe.bind(&context, &state, &mut reg)?;
        contributions = reg.contributions;
    }
    Ok(crate::plugin::prompt::PromptCatalog::new(
        contributions.prompt,
        crate::plugin::prompt::PromptCatalogSnapshot::from_inventory(
            &crate::plugin::InstalledInventory {
                scope: crate::plugin::InventoryScope::Deployment(
                    crate::plugin::RegistryKind::Session,
                ),
                readiness: vec![crate::plugin::ContributionReadiness::NotValidated; entries.len()],
                entries,
            },
        ),
    ))
}
