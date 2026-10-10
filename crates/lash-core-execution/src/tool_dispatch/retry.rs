use crate::{ExecutionPolicy, PreparedToolCall, ToolContext, ToolOutcome};
use futures_util::FutureExt as _;

use super::atomic_attempt::AttemptAuthority;
use super::context::{ToolDispatchContext, ToolDispatchOutcome};

pub(crate) fn resolve_execution_policy(
    context: &ToolDispatchContext<'_>,
    tool_id: &crate::ToolId,
    execution_grant: Option<&crate::ToolExecutionGrant>,
) -> ExecutionPolicy {
    execution_grant
        .map(|grant| grant.manifest().execution_policy)
        .or_else(|| {
            super::preparation::resolve_callable_manifest_by_id(context, tool_id)
                .map(|manifest| manifest.execution_policy)
        })
        .unwrap_or(ExecutionPolicy::Once)
}

/// Runs one attempt of `prepared` with its attempt number stamped on the
/// context; the call keeps its id across every attempt.
pub(super) async fn execute_leaf_tool_attempt<'run>(
    context: &ToolDispatchContext<'run>,
    authority: &AttemptAuthority<'_>,
    prepared: &PreparedToolCall,
    tool_context: ToolContext<'run>,
    attempt: u32,
    max_attempts: u32,
) -> crate::ToolAttemptOutcome {
    execute_once_with_authority(
        context,
        authority,
        prepared,
        tool_context.with_attempt(attempt, max_attempts),
    )
    .await
}

/// Runs a leaf tool body exactly once, with no retry ladder around it.
///
/// This compatibility entry point resolves authority before entering the
/// shared implementation so tests exercise the production admission path.
#[cfg(any(test, feature = "testing"))]
pub async fn execute_once<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: &PreparedToolCall,
    tool_context: ToolContext<'run>,
    grant: Option<&crate::ToolExecutionGrant>,
) -> crate::ToolAttemptOutcome {
    let Some(authority) = AttemptAuthority::resolve(context, &prepared.tool_id, grant) else {
        return ToolOutcome::failure(crate::ToolFailure::runtime(
            crate::ToolFailureClass::Unavailable,
            "tool_unavailable",
            "Tool is unavailable in this session",
        ))
        .into();
    };
    Box::pin(execute_once_with_authority(
        context,
        &authority,
        prepared,
        tool_context,
    ))
    .await
}

async fn execute_once_with_authority<'run>(
    context: &ToolDispatchContext<'run>,
    authority: &AttemptAuthority<'_>,
    prepared: &PreparedToolCall,
    tool_context: ToolContext<'run>,
) -> crate::ToolAttemptOutcome {
    match build_attempt_context(&tool_context, authority.manifest()).await {
        Ok(attempt_context) => {
            execute_attempt_body(context, authority.manifest(), prepared, &attempt_context).await
        }
        Err(result) => result.into(),
    }
}

async fn execute_attempt_body(
    context: &ToolDispatchContext<'_>,
    manifest: &crate::ToolManifest,
    prepared: &PreparedToolCall,
    attempt_context: &crate::AttemptContext<'_>,
) -> crate::ToolAttemptOutcome {
    std::panic::AssertUnwindSafe(async {
        context
            .tools
            .execute(crate::ToolCall::new(
                manifest,
                &prepared.args,
                attempt_context,
            ))
            .await
    })
    .catch_unwind()
    .await
    .unwrap_or_else(|payload| tool_panicked(prepared, payload).into())
}

async fn build_attempt_context<'run>(
    tool_context: &ToolContext<'run>,
    admitted: &crate::ToolManifest,
) -> Result<crate::AttemptContext<'run>, ToolOutcome> {
    let scoped = tool_context.effect_controller.clone();
    // The key is reserved before the body runs, and only for a declared
    // deferrer on a controller that can route await events across process
    // loss. Report which of the two is missing rather than blaming the
    // controller for a tool whose admitted declaration never claimed the
    // capability.
    let completion = match tool_context.completion.load() {
        Some(key) => crate::tool_provider::AttemptCompletionSupport::Available(key),
        None if !admitted.declaration().may_defer => {
            crate::tool_provider::AttemptCompletionSupport::NotDeclared
        }
        None => crate::tool_provider::AttemptCompletionSupport::ControllerUnsupported,
    };
    Ok(crate::AttemptContext::from_tool_context(
        tool_context,
        scoped.scope_id().to_string(),
        completion,
    ))
}

fn tool_panicked(
    prepared: &PreparedToolCall,
    payload: Box<dyn std::any::Any + Send>,
) -> ToolOutcome {
    let message = crate::panic_containment::payload_message(payload.as_ref());
    ToolOutcome::failure(crate::ToolFailure::runtime(
        crate::ToolFailureClass::Internal,
        "tool_panicked",
        "The tool panicked. Outside work may already have happened; check outside state before calling again.",
    ).with_cause(crate::ToolFailureCause::Panicked {
        tool_name: prepared.tool_name.clone(),
        call_id: prepared.call_id.clone(),
        message,
    }))
}

/// A completed tool output ready to record; its producer has already put attachments.
///
/// Its payload is private to this module so record construction cannot accept a raw
/// [`ToolOutcome`] from a tool body or plugin hook.
pub(super) struct NormalizedToolOutput(crate::ToolCallOutput);

impl NormalizedToolOutput {
    pub(super) fn into_output(self) -> crate::ToolCallOutput {
        self.0
    }
}

pub(crate) async fn normalized_outcome(
    _context: &ToolDispatchContext<'_>,
    ids: &super::context::ToolCallIds,
    tool_name: String,
    args: serde_json::Value,
    result: ToolOutcome,
) -> ToolDispatchOutcome {
    let output = NormalizedToolOutput(result.into_done_output().unwrap_or_else(|_| {
        crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
            crate::ToolFailureClass::Internal,
            "pending_tool_not_finalized",
            "pending tool result reached a completed-output projection path",
        ))
    }));
    super::context::outcome(ids, tool_name, args, output)
}

/// Settles a tool call that parked and has now been resolved.
///
/// Extracted from `RuntimeExecutionContext::pending_completion_dispatch_outcome`
/// verbatim, because the handler-level invocation driver (ADR 0099 §2,
/// FIG-2266) awaits its own deferred completions and holds no
/// `RuntimeExecutionContext` to ask — §3 forbids carrying one across the
/// handler boundary. Every input this body ever used came from the dispatch
/// context, so the two callers share one settlement rather than each spelling
/// the projection, the after-tool hook and the trailing trace attempt.
///
/// `prepared` is the parked call as admitted, so the result phase of its
/// Deferred completion inspects the call that executed. `attempts` are the
/// attempts before the one that parked.
pub(crate) async fn settle_completed_pending_tool_call(
    context: &ToolDispatchContext<'_>,
    ids: &super::context::ToolCallIds,
    prepared: &crate::plugin::PreparedCallReadView,
    resolution: crate::Resolution,
    resolver: Option<&crate::PendingResolver>,
    attempts: Vec<lash_trace::TraceRetryAttempt>,
) -> ToolDispatchOutcome {
    let output = crate::tool_result::tool_output_from_completion_resolution(resolution, resolver);
    let parked_attempt = u32::try_from(attempts.len())
        .unwrap_or(u32::MAX)
        .saturating_add(1);
    let result = super::finalize_tool_result_with_execution_context(
        context,
        prepared,
        super::deferred_occurrence(parked_attempt),
        ToolOutcome::from_output(output),
    )
    .await;
    let tool_name = prepared.tool_name().to_string();
    let result =
        match super::preparation::resolve_callable_manifest_by_id(context, prepared.tool_id()) {
            Some(manifest) => {
                super::atomic_attempt::settled_outcome(result, manifest.declaration(), &tool_name)
            }
            None => result,
        };
    let args = prepared.args().clone();
    let mut outcome = normalized_outcome(context, ids, tool_name, args, result).await;
    let mut attempts = attempts;
    attempts.push(crate::trace::trace_tool_attempt(
        attempts
            .len()
            .saturating_add(1)
            .try_into()
            .unwrap_or(u32::MAX),
        &outcome.record,
        None,
    ));
    outcome.attempts = attempts;
    outcome
}
