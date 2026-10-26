use crate::PreparedToolCall;
use crate::ToolContext;
use crate::ToolOutcome;
use futures_util::FutureExt as _;
use lash_kernel_value::tool_contract::ExecutionPolicy;

use super::atomic_attempt::AttemptAuthority;
use super::context::{ToolDispatchContext, ToolDispatchOutcome};

pub(crate) fn resolve_execution_policy(
    context: &ToolDispatchContext<'_>,
    tool_id: &lash_vocabulary::tool_contract::ToolId,
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
            let (trace, body) = attempt_trace(&tool_context, authority);
            let attempt_context = attempt_context.with_trace(trace);
            let _completion_fence = attempt_context.live().fence();
            execute_attempt_body(context, authority, prepared, &attempt_context, body).await
        }
        Err(result) => result.into(),
    }
}

/// The capability of one attempt body and its guard: under the call's tool
/// span when this execution traced the call's start, else untraced. It is
/// bound here from the granted provider, or the registered provider the
/// call's admitted binding resolved to; else by the route that resolves
/// it.
fn attempt_trace(
    tool_context: &ToolContext<'_>,
    authority: &AttemptAuthority<'_>,
) -> (crate::trace::ExtensionTrace, crate::trace::BodyTrace) {
    let spec = tool_context
        .runtime_execution_context
        .as_ref()
        .and_then(|runtime| {
            runtime.tool_body_spec(
                crate::CallAddress {
                    owner: tool_context.owner.clone(),
                    call_id: tool_context.call_id.clone(),
                    attempt: tool_context.attempt_number,
                },
                tool_context.live.as_ref(),
            )
        });
    let Some(spec) = spec else {
        return (
            crate::trace::ExtensionTrace::untraced(),
            crate::trace::BodyTrace::untraced(),
        );
    };
    let (trace, body) = spec.bind();
    let provider = match authority {
        AttemptAuthority::Granted(grant) => Some(grant.provider().callback_identity()),
        AttemptAuthority::Catalog { .. } => tool_context.executable.clone(),
    };
    if let Some(callback) = provider.as_ref().and_then(crate::trace::BoundCallback::of) {
        body.bind(callback);
    }
    (trace, body)
}

async fn execute_attempt_body(
    context: &ToolDispatchContext<'_>,
    authority: &AttemptAuthority<'_>,
    prepared: &PreparedToolCall,
    attempt_context: &crate::AttemptContext<'_>,
    body: crate::trace::BodyTrace,
) -> crate::ToolAttemptOutcome {
    // The body polls under its position, so instrumented libraries in it
    // nest under its tool span; its exit, however it exits, revokes it.
    let outcome =
        std::panic::AssertUnwindSafe(body.guard(attempt_context.trace().instrument(async {
            context
                .tools
                .execute(crate::ToolCall::new(
                    authority.manifest(),
                    authority.contract(),
                    &prepared.args,
                    attempt_context,
                ))
                .await
        })))
        .catch_unwind()
        .await
        .unwrap_or_else(|payload| tool_panicked(prepared, payload).into());
    match unheld_launch(&outcome, attempt_context) {
        Some(env_ref) => launch_not_held(&env_ref).into(),
        None => outcome,
    }
}

/// The first environment a start `outcome` declares that `attempt` did not
/// hold through [`AttemptContext::declare_launch`](crate::AttemptContext::declare_launch).
///
/// Both attempt paths run every body here, before its outcome is recorded:
/// a declaration a body built or decoded without holding its launch is the
/// call's failure, and nothing it declared is realized (D10).
fn unheld_launch(
    outcome: &crate::ToolAttemptOutcome,
    attempt: &crate::AttemptContext<'_>,
) -> Option<crate::ProcessEnvRef> {
    let starts: Vec<&lash_execution_contracts::tool_intent::StartProcessIntent> = match outcome {
        crate::ToolAttemptOutcome::Done { intents, .. } => intents
            .intents
            .iter()
            .filter_map(|intent| match intent {
                lash_execution_contracts::tool_intent::ToolIntent::StartProcess(start) => {
                    Some(start.as_ref())
                }
                _ => None,
            })
            .collect(),
        crate::ToolAttemptOutcome::Pending(completion) => match &completion.resolved_by {
            Some(crate::PendingResolver::DeclaredStart(start)) => vec![start.start()],
            _ => Vec::new(),
        },
        crate::ToolAttemptOutcome::HostFailed(_) => Vec::new(),
    };
    starts
        .into_iter()
        .filter_map(|start| start.declaration.input().launch())
        .map(|launch| &launch.env)
        .find(|env| !attempt.held_launch(env))
        .cloned()
}

fn launch_not_held(env_ref: &crate::ProcessEnvRef) -> ToolOutcome {
    ToolOutcome::failure(
        crate::ToolFailure::runtime(
            crate::ToolFailureClass::Internal,
            "process_launch_not_held",
            format!(
                "a declared start names process environment `{env_ref}`, which this attempt did not hold"
            ),
        )
        .with_cause(crate::ToolFailureCause::LaunchNotHeld {
            env_ref: env_ref.to_string(),
        }),
    )
}

async fn build_attempt_context<'run>(
    tool_context: &ToolContext<'run>,
    admitted: &lash_vocabulary::tool_contract::ToolManifest,
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
