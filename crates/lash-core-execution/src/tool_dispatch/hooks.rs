//! The dispatch side of the tool hook composition (ADR 0128): the context
//! every tool hook of a call reads, the before-checks over the prepared
//! call, and the result phase every completed result passes through.

use std::sync::Arc;

use lash_core_store::tool_run::{AttributedVerdict, CheckRecord, RankedVerdict};

use crate::plugin::{
    AfterToolDecision, BeforeSelection, BeforeToolDecision, PreparedCallReadView, ToolHookContext,
    ToolHookOccurrence, ToolResultCandidate,
};
use crate::{ToolCallOutput, ToolOutcome};

use super::context::ToolDispatchContext;

/// The context every tool hook of `prepared`'s call reads.
pub(super) fn hook_context(
    context: &ToolDispatchContext<'_>,
    call_id: &crate::ToolCallId,
    tool_id: &crate::ToolId,
    tool_name: &str,
    argument_projection: crate::ToolArgumentProjectionPolicy,
) -> ToolHookContext {
    ToolHookContext {
        owner: context.owner.runtime_owner(),
        call_id: call_id.clone(),
        tool_id: tool_id.clone(),
        tool_name: tool_name.to_string(),
        plugin_config: context.plugins.admitted_plugin_config(),
        argument_projection,
        turn_context: context.turn_context.clone(),
        sessions: Arc::clone(&context.sessions),
    }
}

/// Run every before-check over the one prepared call, publish the evidence
/// of any terminal reply the reduction displaced, and return what admission
/// selects.
pub(super) async fn check_prepared_call(
    context: &ToolDispatchContext<'_>,
    hook_context: &ToolHookContext,
    original_args: &Arc<serde_json::Value>,
    prepared: &PreparedCallReadView,
) -> BeforeSelection {
    let record = match context
        .plugins
        .check_tool_args(hook_context, original_args, prepared)
        .await
    {
        Ok(record) => record,
        Err(failure) => {
            return BeforeSelection::Terminal(ToolCallOutput::failure(*failure));
        }
    };
    publish_check_evidence(context, "tool_args_check", &record, before_kind).await;
    crate::plugin::before_selection(&record)
}

/// Pass a completed result through the result transforms and after-checks:
/// the result phase of an executed attempt, a Deferred completion or a
/// cached success. A still-pending result is not a final result and passes
/// through untouched.
pub async fn finalize_tool_result_with_execution_context(
    context: &ToolDispatchContext<'_>,
    prepared: &PreparedCallReadView,
    occurrence: ToolHookOccurrence,
    result: ToolOutcome,
) -> ToolOutcome {
    if !context.plugins.has_tool_result_hooks() {
        return result;
    }
    let output = match result.into_done_output() {
        Ok(output) => output,
        Err(pending) => return ToolOutcome::pending(pending),
    };
    let (original, control) = ToolResultCandidate::split(output);
    let argument_projection =
        super::preparation::resolve_callable_manifest_by_id(context, prepared.tool_id())
            .map(|manifest| manifest.argument_projection)
            .unwrap_or_default();
    let hook_context = hook_context(
        context,
        prepared.call_id(),
        prepared.tool_id(),
        prepared.tool_name(),
        argument_projection,
    );
    let original = Arc::new(original);
    let candidate = match context
        .plugins
        .transform_tool_result(&hook_context, occurrence, prepared, &original)
        .await
    {
        Ok(candidate) => candidate,
        Err(failure) => return ToolOutcome::failure(*failure),
    };
    let final_result = Arc::new(candidate);
    let checks = match context
        .plugins
        .check_tool_result(
            &hook_context,
            occurrence,
            prepared,
            &original,
            &final_result,
        )
        .await
    {
        Ok(checks) => checks,
        Err(failure) => return ToolOutcome::failure(*failure),
    };
    let mut observations = context.observation_cursor("checks:after");
    for contribution in checks.contributions {
        crate::plugin::observe_plugin_runtime_events(
            &mut observations,
            context.observer.as_ref(),
            &contribution.plugin_id,
            contribution.events,
        );
    }
    publish_check_evidence(context, "tool_result_check", &checks.record, after_kind).await;
    if let Err(failure) = carry_result_check_state(context, checks.proposals) {
        return ToolOutcome::failure(*failure);
    }
    let final_result = Arc::unwrap_or_clone(final_result);
    ToolOutcome::from_output(crate::plugin::after_resolution(
        final_result,
        control,
        &checks.record,
    ))
}

/// Commands need the recorded body that owns their decision. Without it,
/// refuse the result rather than journal state separately from eligibility.
fn carry_result_check_state(
    context: &ToolDispatchContext<'_>,
    proposals: Vec<crate::plugin::Proposal>,
) -> Result<(), Box<crate::ToolFailure>> {
    let Some(first) = proposals.first() else {
        return Ok(());
    };
    let plugin = first.batch.plugin.plugin.clone();
    crate::plugin::propose_all(&context.plugins, proposals).map_err(|error| {
        let mut failure = crate::ToolFailure::runtime(
            crate::ToolFailureClass::Internal,
            "tool_result_check_state_unrecorded",
            error.to_string(),
        )
        .with_cause(crate::ToolFailureCause::PluginStateUnrecorded { plugin });
        failure.source = crate::ToolFailureSource::Plugin;
        Box::new(failure)
    })
}

/// The occurrence of a completed attempt's result.
pub(crate) fn attempt_occurrence(attempt: u32) -> ToolHookOccurrence {
    ToolHookOccurrence::Attempt {
        attempt: lash_core_store::tool_run::AttemptOrdinal::new(attempt)
            .unwrap_or(lash_core_store::tool_run::AttemptOrdinal::FIRST),
    }
}

/// The occurrence of a Deferred completion that parked `attempt`.
pub(crate) fn deferred_occurrence(attempt: u32) -> ToolHookOccurrence {
    ToolHookOccurrence::DeferredCompletion {
        attempt: lash_core_store::tool_run::AttemptOrdinal::new(attempt)
            .unwrap_or(lash_core_store::tool_run::AttemptOrdinal::FIRST),
    }
}

fn before_kind(verdict: &BeforeToolDecision) -> &'static str {
    match verdict {
        BeforeToolDecision::Allow => "allow",
        BeforeToolDecision::Cached(_) => "cached",
        BeforeToolDecision::Deny(_) => "deny",
        BeforeToolDecision::Cancel(_) => "cancel",
        BeforeToolDecision::AbortRun(_) => "abort_run",
    }
}

fn after_kind(verdict: &AfterToolDecision) -> &'static str {
    match verdict {
        AfterToolDecision::Allow => "allow",
        AfterToolDecision::Deny(_) => "deny",
        AfterToolDecision::Cancel(_) => "cancel",
        AfterToolDecision::AbortRun(_) => "abort_run",
    }
}

fn attributed_json<V>(
    reply: &AttributedVerdict<V>,
    kind: fn(&V) -> &'static str,
) -> serde_json::Value {
    serde_json::json!({
        "plugin_id": reply.callback.owner.plugin,
        "callback": reply.callback.key,
        "verdict": kind(&reply.verdict),
    })
}

/// When a reduction selected one terminal reply over others, publish the
/// winner and every displaced terminal, in reduction order, as one
/// composition event attributed to the winner's plugin.
async fn publish_check_evidence<V: RankedVerdict>(
    context: &ToolDispatchContext<'_>,
    phase: &'static str,
    record: &CheckRecord<V>,
    kind: fn(&V) -> &'static str,
) {
    let displaced = crate::plugin::displaced_terminals(record);
    let Some(winner) = record.winner().filter(|_| !displaced.is_empty()) else {
        return;
    };
    let name = format!("{phase}.conflict");
    let payload = serde_json::json!({
        "winner": attributed_json(winner, kind),
        "displaced": displaced
            .iter()
            .map(|reply| attributed_json(reply, kind))
            .collect::<Vec<_>>(),
    });
    let plugin_id = winner.callback.owner.plugin.as_str();
    if let Err(error) = context
        .session_graph
        .emit_trace_event(
            crate::plugin::owner_trace_context(&context.owner.runtime_owner()),
            lash_trace::TraceEvent::Custom {
                name: format!("plugin.{plugin_id}.{name}"),
                payload: payload.clone(),
            },
        )
        .await
    {
        tracing::error!(
            target: "lash::plugin_composition",
            plugin_id,
            error = %error,
            phase,
            "failed to emit tool check conflict trace"
        );
    }
    context
        .observation_cursor(&format!("checks:{phase}"))
        .observe(
            context.observer.as_ref(),
            crate::engine::ObservedEvent::Session(crate::SessionStreamEvent::PluginEvent {
                plugin_id: plugin_id.to_string(),
                event: crate::PluginRuntimeEvent::Custom { name, payload },
            }),
        );
}
