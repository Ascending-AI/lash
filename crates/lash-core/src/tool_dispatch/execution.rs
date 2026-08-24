use std::sync::Arc;

use crate::plugin::ToolResultHookContext;
use crate::{PreparedToolCall, ToolContext, ToolFailureClass, ToolManifest, ToolOutcome};
use futures_util::FutureExt as _;

use super::context::ToolDispatchOutcome;
use super::context::{
    PendingToolDispatchOutcome, ToolCallLaunch, ToolDispatchContext, launch_done, outcome,
    runtime_failure,
};
use super::directives::apply_after_tool_directives;
use super::retry::execute_leaf_tool_attempt;

/// The authority a prepared tool attempt runs under, resolved once at dispatch
/// entry and then carried through the single attempt path.
///
/// A model-facing call is authorized by Tool Catalog membership; a
/// deferred-resolution call carries its own out-of-catalog
/// [`crate::ToolExecutionGrant`]. Both admit exactly one tool manifest, and
/// every downstream difference between the two routes — the identifier guard,
/// the execution binding, the execution seam, the attachment producer name —
/// is a method on this value rather than a forked dispatch body.
pub(super) enum AttemptAuthority<'grant> {
    /// Tool Catalog membership admitted the call; the manifest was resolved by
    /// the prepared tool id.
    Catalog(Box<ToolManifest>),
    /// An execution grant admitted the call. The grant *is* the authority, so
    /// the catalog lookup is skipped entirely.
    Granted(&'grant crate::ToolExecutionGrant),
}

impl<'grant> AttemptAuthority<'grant> {
    /// Resolves the authority for one prepared attempt.
    ///
    /// A grant short-circuits the catalog lookup: out-of-catalog execution is
    /// the whole point of a grant, so a grant never has to be a catalog member.
    /// Without a grant, catalog membership is the only admission route, and a
    /// non-member resolves to `None`.
    pub(super) fn resolve(
        context: &ToolDispatchContext<'_>,
        tool_id: &crate::ToolId,
        grant: Option<&'grant crate::ToolExecutionGrant>,
    ) -> Option<Self> {
        match grant {
            Some(grant) => Some(Self::Granted(grant)),
            None => super::preparation::resolve_callable_manifest_by_id(context, tool_id)
                .map(|manifest| Self::Catalog(Box::new(manifest))),
        }
    }

    pub(super) fn manifest(&self) -> &ToolManifest {
        match self {
            Self::Catalog(manifest) => manifest,
            Self::Granted(grant) => grant.manifest(),
        }
    }

    pub(super) fn grant(&self) -> Option<&'grant crate::ToolExecutionGrant> {
        match self {
            Self::Catalog(_) => None,
            Self::Granted(grant) => Some(grant),
        }
    }

    /// Refuses a prepared call whose identifier does not match the manifest the
    /// authority admitted.
    ///
    /// The check is grant-only by construction, not by omission: the catalog
    /// arm resolves its manifest *by* the prepared tool id, so the two can
    /// never disagree there. Under a grant the manifest arrives from the grant
    /// while the prepared call arrives from the provider, so the pair really
    /// can drift and the mismatch has to be refused.
    fn verify_prepared_identity(&self, prepared: &PreparedToolCall) -> Result<(), ToolOutcome> {
        let Self::Granted(grant) = self else {
            return Ok(());
        };
        if prepared.tool_id == grant.manifest().id {
            return Ok(());
        }
        Err(runtime_failure(
            ToolFailureClass::Internal,
            "granted_tool_id_mismatch",
            format!(
                "Prepared granted tool id `{}` does not match grant id `{}`",
                prepared.tool_id,
                grant.manifest().id
            ),
        ))
    }

    /// Applies the grant's execution binding to the tool context.
    ///
    /// Only a grant carries a binding, and it must be applied before the
    /// completion context is cloned so a parking attempt keeps it.
    fn apply_execution_binding<'run>(&self, tool_context: ToolContext<'run>) -> ToolContext<'run> {
        match self {
            Self::Catalog(_) => tool_context,
            Self::Granted(grant) => {
                tool_context.with_tool_execution_binding(grant.execution_binding.clone())
            }
        }
    }
}

/// Appends the process event a deferring attempt declared, at the moment the
/// call parks.
///
/// A recorded attempt body cannot append process events — its
/// [`crate::AttemptContext`] has no route to them — so a tool that must
/// announce its durable wait declares the event on its
/// [`crate::PendingCompletion`] instead. The append happens here, after the
/// completion key is taken and before the call is handed back as pending, so
/// the announcement cannot exist without the park it announces. A failed
/// append fails the call rather than parking silently.
///
/// The declaration is dropped from the returned policy: it has been executed,
/// and nothing downstream may replay it out of this seam.
async fn announce_pending_park(
    context: &ToolContext<'_>,
    mut pending: crate::PendingCompletion,
) -> Result<crate::PendingCompletion, ToolOutcome> {
    let Some(announcement) = pending.announcement.take() else {
        return Ok(pending);
    };
    match context
        .process_events()
        .emit_request(announcement.into_append_request())
        .await
    {
        Ok(_) => Ok(pending),
        Err(err) => Err(runtime_failure(
            ToolFailureClass::Internal,
            "pending_tool_announcement_failed",
            format!("declared park announcement could not be appended: {err}"),
        )),
    }
}

/// Runs an authored process-replay tool body without creating a ToolAttempt
/// frame. Any durable operations the body issues are consequently direct
/// children of the enclosing process replay and must be awaited by the body.
pub(crate) async fn execute_orchestrating_tool<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: PreparedToolCall,
    tool_context: ToolContext<'run>,
) -> ToolDispatchOutcome {
    let started = context.clock.now();
    let tool_name = prepared.tool_name.clone();
    let args = prepared.args.clone();
    let tool_context = tool_context.with_prepared_payload(prepared.prepared_payload.clone());
    let orchestration_context =
        crate::tool_provider::orchestration::OrchestrationContext::new(tool_context);
    let result = std::panic::AssertUnwindSafe(async {
        let Some(registry) = context.tool_registry.as_deref() else {
            return ToolOutcome::err_fmt("orchestrating registration is missing its tool registry");
        };
        registry
            .execute_orchestrating_by_id(&prepared.tool_id, &prepared.args, &orchestration_context)
            .await
    })
    .catch_unwind()
    .await
    .unwrap_or_else(|payload| {
        let message = crate::panic_containment::payload_message(payload.as_ref());
        crate::panic_containment::enforce_loudness(payload);
        ToolOutcome::failure(crate::ToolFailure::runtime(
            ToolFailureClass::Internal,
            "tool_panicked",
            message,
        ))
    });
    let duration_ms = context.clock.now().duration_since(started).as_millis() as u64;
    let result = finalize_tool_result_with_execution_context(
        context,
        &tool_name,
        &args,
        result,
        duration_ms,
    )
    .await;
    let output = match result {
        ToolOutcome::Done(output) => *output,
        ToolOutcome::Pending(_) => crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
            ToolFailureClass::Internal,
            "orchestrating_tool_returned_pending",
            "orchestrating tools must immediately await journaled actions and return a completed result",
        )),
    };
    ToolDispatchOutcome {
        record: crate::ToolCallRecord {
            call_id: Some(prepared.call_id),
            tool: tool_name,
            args,
            output,
            duration_ms,
        },
        attempts: Vec::new(),
        intents: crate::ToolIntents::default(),
        intent_outcomes: Vec::new(),
    }
}

/// Runs an internal process-body tool without creating a `ToolAttempt` frame.
///
/// Unlike authored orchestration, this is the owner-bound activity of the
/// process itself and may perform host I/O. It is available only to
/// `ToolActivation::Internal` process inputs, never to model-facing calls.
pub(crate) async fn execute_internal_process_tool<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: PreparedToolCall,
    tool_context: ToolContext<'run>,
) -> ToolDispatchOutcome {
    let started = context.clock.now();
    let tool_name = prepared.tool_name.clone();
    let args = prepared.args.clone();
    let tool_context = tool_context.with_prepared_payload(prepared.prepared_payload.clone());
    let internal_context = crate::InternalProcessContext::new(tool_context);
    let result = std::panic::AssertUnwindSafe(context.tools.execute_internal_by_id(
        &prepared.tool_id,
        &prepared.args,
        &internal_context,
    ))
    .catch_unwind()
    .await
    .unwrap_or_else(|payload| {
        let message = crate::panic_containment::payload_message(payload.as_ref());
        crate::panic_containment::enforce_loudness(payload);
        ToolOutcome::failure(crate::ToolFailure::runtime(
            ToolFailureClass::Internal,
            "tool_panicked",
            message,
        ))
    });
    let duration_ms = context.clock.now().duration_since(started).as_millis() as u64;
    let result = finalize_tool_result_with_execution_context(
        context,
        &tool_name,
        &args,
        result,
        duration_ms,
    )
    .await;
    let output = match result {
        ToolOutcome::Done(output) => *output,
        ToolOutcome::Pending(_) => crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
            ToolFailureClass::Internal,
            "internal_process_tool_returned_pending",
            "internal process-body tools must return a completed result",
        )),
    };
    ToolDispatchOutcome {
        record: crate::ToolCallRecord {
            call_id: Some(prepared.call_id),
            tool: tool_name,
            args,
            output,
            duration_ms,
        },
        attempts: Vec::new(),
        intents: crate::ToolIntents::default(),
        intent_outcomes: Vec::new(),
    }
}

#[cfg(test)]
pub(crate) async fn dispatch_prepared_tool_call_with_execution_context<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: PreparedToolCall,
    tool_context: ToolContext<'run>,
) -> ToolDispatchOutcome {
    coordinate_prepared_tool_call_launch_with_execution_context(
        context,
        prepared,
        None,
        tool_context,
    )
    .await
    .into_done_or_runtime_failure()
}

#[cfg(test)]
pub(crate) async fn coordinate_prepared_tool_call_launch_with_execution_context<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: PreparedToolCall,
    execution_grant: Option<Box<crate::ToolExecutionGrant>>,
    tool_context: ToolContext<'run>,
) -> ToolCallLaunch {
    let retry_policy =
        super::retry::resolve_retry_policy(context, &prepared.tool_id, execution_grant.as_deref());
    let cancellation = tool_context.cancellation_token().cloned();
    let dispatch = Arc::new(context.clone());
    super::coordinate_tool_invocation(
        context,
        prepared,
        execution_grant,
        retry_policy,
        super::ToolAttemptEffectIdentity::Scalar {
            parent: context.parent_invocation.clone(),
        },
        cancellation,
        None,
        None,
        |completion_key| {
            crate::RuntimeEffectLocalExecutor::prepared_tool_attempt(
                Arc::clone(&dispatch),
                tool_context.clone(),
                completion_key,
            )
        },
    )
    .await
    .launch
}

/// Launches one prepared tool attempt under whichever authority admitted it.
///
/// Catalog calls and granted calls share this body: the authority is resolved
/// once at entry, and the two routes differ only through
/// [`AttemptAuthority`]'s methods.
pub(super) async fn dispatch_prepared_tool_attempt_launch_with_execution_context<'run>(
    context: &ToolDispatchContext<'run>,
    grant: Option<&crate::ToolExecutionGrant>,
    prepared: PreparedToolCall,
    attempt: u32,
    max_attempts: u32,
    tool_context: ToolContext<'run>,
) -> ToolCallLaunch {
    let args = prepared.args.clone();
    let Some(authority) = AttemptAuthority::resolve(context, &prepared.tool_id, grant) else {
        return launch_done(outcome(
            prepared.tool_name.clone(),
            args,
            runtime_failure(
                ToolFailureClass::Unavailable,
                "tool_unavailable",
                "Tool is unavailable in this session",
            ),
            0,
        ));
    };
    let tool_name = authority.manifest().name.clone();
    if let Err(failure) = authority.verify_prepared_identity(&prepared) {
        return launch_done(outcome(tool_name, args, failure, 0));
    }

    let tool_start = context.clock.now();
    let tool_context = authority.apply_execution_binding(
        tool_context.with_prepared_payload(prepared.prepared_payload.clone()),
    );
    let completion_context = tool_context.clone();
    let attempt_result = execute_leaf_tool_attempt(
        context,
        &authority,
        &prepared,
        tool_context,
        attempt,
        max_attempts,
    )
    .await;
    let duration_ms = context.clock.now().duration_since(tool_start).as_millis() as u64;
    let (result, intents) = match attempt_result {
        crate::ToolAttemptOutcome::Done { result, intents } => {
            (ToolOutcome::from_output(result.into_output()), intents)
        }
        crate::ToolAttemptOutcome::Pending(pending) => {
            let key = match completion_context.take_completion_key() {
                Some(key) => key,
                None => {
                    return launch_done(outcome(
                        tool_name,
                        args,
                        runtime_failure(
                            ToolFailureClass::Internal,
                            "pending_tool_missing_completion_key",
                            "tool returned Pending without first obtaining a completion key",
                        ),
                        duration_ms,
                    ));
                }
            };
            let pending = match announce_pending_park(&completion_context, pending).await {
                Ok(pending) => pending,
                Err(failure) => {
                    return launch_done(outcome(tool_name, args, failure, duration_ms));
                }
            };
            return ToolCallLaunch::Pending(Box::new(PendingToolDispatchOutcome {
                tool_name,
                args,
                key,
                pending,
                duration_ms,
                attempts: Vec::new(),
            }));
        }
    };

    let result = finalize_tool_result_with_execution_context(
        context,
        &tool_name,
        &args,
        result,
        duration_ms,
    )
    .await;

    let mut outcome = outcome(tool_name, args, result, duration_ms);
    outcome.intents = intents;
    launch_done(outcome)
}

pub(crate) async fn execute_prepared_tool_attempt_effect<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: PreparedToolCall,
    execution_grant: Option<Box<crate::ToolExecutionGrant>>,
    attempt: u32,
    max_attempts: u32,
    tool_context: ToolContext<'run>,
) -> Result<crate::ToolAttemptEffectOutcome, crate::RuntimeEffectControllerError> {
    let call_id = prepared.call_id.clone();
    let launch = Box::pin(
        dispatch_prepared_tool_attempt_launch_with_execution_context(
            context,
            execution_grant.as_deref(),
            prepared,
            attempt,
            max_attempts,
            tool_context,
        ),
    )
    .await;
    let launch = match launch {
        ToolCallLaunch::Done(outcome) => {
            let mut record = outcome.record;
            record.call_id = Some(call_id);
            crate::ToolAttemptLaunch::Done {
                record: Box::new(record),
                intents: outcome.intents,
            }
        }
        ToolCallLaunch::Pending(pending) => crate::ToolAttemptLaunch::Pending {
            key: Box::new(pending.key),
            pending: pending.pending,
            duration_ms: pending.duration_ms,
        },
    };
    let triggers = context.trigger_outcomes.drain();
    Ok(crate::ToolAttemptEffectOutcome { launch, triggers })
}

pub(crate) async fn finalize_tool_result_with_execution_context(
    context: &ToolDispatchContext<'_>,
    tool_name: &str,
    args: &serde_json::Value,
    result: ToolOutcome,
    duration_ms: u64,
) -> ToolOutcome {
    match context
        .plugins
        .after_tool_call(ToolResultHookContext::new(
            context.session_id.clone(),
            tool_name.to_string(),
            args.clone(),
            result.clone(),
            duration_ms,
            context.turn_context.clone(),
            Arc::clone(&context.sessions),
        ))
        .await
    {
        Ok(directives) => Box::pin(apply_after_tool_directives(context, result, directives)).await,
        Err(err) => runtime_failure(
            ToolFailureClass::Internal,
            "after_tool_call_failed",
            err.to_string(),
        ),
    }
}

#[cfg(test)]
trait ToolCallLaunchExt {
    fn into_done_or_runtime_failure(self) -> ToolDispatchOutcome;
}

#[cfg(test)]
impl ToolCallLaunchExt for ToolCallLaunch {
    fn into_done_or_runtime_failure(self) -> ToolDispatchOutcome {
        match self {
            ToolCallLaunch::Done(outcome) => *outcome,
            ToolCallLaunch::Pending(pending) => outcome(
                pending.tool_name,
                pending.args,
                runtime_failure(
                    ToolFailureClass::Internal,
                    "pending_tool_not_supported_here",
                    "pending tool completion is not supported on this dispatch path",
                ),
                pending.duration_ms,
            ),
        }
    }
}
