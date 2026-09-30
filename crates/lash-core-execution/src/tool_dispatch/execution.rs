use std::sync::Arc;

use crate::plugin::ToolResultHookContext;
use crate::{PreparedToolCall, ToolContext, ToolFailureClass, ToolManifest, ToolOutcome};

use super::context::{ToolCallIds, ToolDispatchContext, attempt_done, runtime_failure};
#[cfg(any(test, feature = "testing"))]
use super::context::{ToolCallLaunch, ToolDispatchOutcome};
use super::directives::apply_after_tool_directives;
use super::retry::{execute_leaf_tool_attempt, normalized_outcome};

/// The authority a prepared tool attempt runs under, resolved once at dispatch
/// entry and then carried through the single attempt path.
///
/// A model-facing call is authorized by Tool Catalog membership; a
/// deferred-resolution call carries its own out-of-catalog
/// [`crate::ToolExecutionGrant`]. Both admit exactly one tool manifest, and
/// every downstream difference between the two routes — the identifier guard,
/// the execution binding, the source route, the attachment producer name — is
/// a method on this value rather than a forked dispatch body.
pub(super) enum AttemptAuthority<'grant> {
    /// Tool Catalog membership admitted the call; the manifest was resolved by
    /// the prepared tool id.
    Catalog(Box<ToolManifest>),
    /// An execution grant admitted the call. The grant *is* the authority, so
    /// the catalog lookup is skipped entirely.
    Granted(&'grant crate::ToolExecutionGrant),
}

impl<'grant> AttemptAuthority<'grant> {
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

    /// Only a grant carries a binding, and it must be applied before the
    /// completion context is cloned so a parking attempt keeps it.
    fn apply_execution_binding<'run>(&self, tool_context: ToolContext<'run>) -> ToolContext<'run> {
        match self {
            Self::Catalog(_) => tool_context,
            Self::Granted(grant) => tool_context
                .with_tool_execution_binding(grant.execution_binding.clone())
                .with_granted_source_id(grant.source_id.clone()),
        }
    }
}

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
        .append_process_event(announcement.into_append_request())
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

#[cfg(any(test, feature = "testing"))]
pub(crate) async fn dispatch_prepared_tool_call_with_execution_context<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: PreparedToolCall,
    tool_context: ToolContext<'run>,
) -> ToolDispatchOutcome {
    let ids = ToolCallIds::of(&prepared);
    let launch = coordinate_prepared_tool_call_launch_with_execution_context(
        context,
        prepared,
        None,
        tool_context,
    )
    .await;
    tool_call_launch_into_done_or_runtime_failure(context, &ids, launch).await
}

#[cfg(any(test, feature = "testing"))]
pub async fn coordinate_prepared_tool_call_launch_with_execution_context<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: PreparedToolCall,
    execution_grant: Option<Box<crate::ToolExecutionGrant>>,
    tool_context: ToolContext<'run>,
) -> ToolCallLaunch {
    let retry_policy =
        super::retry::resolve_retry_policy(context, &prepared.tool_id, execution_grant.as_deref());
    let turn_cancel_wait = Box::new(
        context.effect_controller.turn_cancel_wait(
            tool_context
                .cancellation_token()
                .cloned()
                .unwrap_or_default(),
        ),
    );
    let dispatch = Arc::new(context.clone());
    Box::pin(super::coordinate_tool_invocation(
        context,
        prepared,
        execution_grant,
        retry_policy,
        None,
        super::ToolAttemptLineage::from_parent(context.parent_invocation.clone()),
        turn_cancel_wait.as_ref(),
        None,
        |completion_key| {
            crate::RuntimeEffectLocalExecutor::prepared_tool_attempt(
                Arc::clone(&dispatch),
                tool_context.clone(),
                completion_key,
            )
        },
    ))
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
) -> crate::ToolAttemptLaunch {
    let args = prepared.args.clone();
    let ids = ToolCallIds::of(&prepared);
    let Some(authority) = AttemptAuthority::resolve(context, &prepared.tool_id, grant) else {
        return attempt_done(
            normalized_outcome(
                context,
                &ids,
                prepared.tool_name.clone(),
                args,
                runtime_failure(
                    ToolFailureClass::Unavailable,
                    "tool_unavailable",
                    "Tool is unavailable in this session",
                ),
            )
            .await,
        );
    };
    let tool_name = authority.manifest().name.clone();
    if let Err(failure) = authority.verify_prepared_identity(&prepared) {
        return attempt_done(normalized_outcome(context, &ids, tool_name, args, failure).await);
    }

    let tool_context = authority.apply_execution_binding(
        tool_context.with_prepared_payload(prepared.prepared_payload.clone()),
    );
    let completion_context = tool_context.clone();
    // Measured for the result hook's observation input only; nothing journaled
    // may read it.
    let tool_started = context.clock.now();
    let attempt_result = Box::pin(execute_leaf_tool_attempt(
        context,
        &authority,
        &prepared,
        tool_context,
        attempt,
        max_attempts,
    ))
    .await;
    let duration_ms = context.clock.now().duration_since(tool_started).as_millis() as u64;
    let (result, intents) = match attempt_result {
        crate::ToolAttemptOutcome::Done { result, intents } => {
            (ToolOutcome::from_output(result.into_output()), intents)
        }
        crate::ToolAttemptOutcome::Pending(pending) => {
            let key =
                match completion_context.take_completion_key() {
                    Some(key) => key,
                    None => {
                        return attempt_done(normalized_outcome(
                        context,
                        &ids,
                        tool_name,
                        args,
                        runtime_failure(
                            ToolFailureClass::Internal,
                            "pending_tool_missing_completion_key",
                            "tool returned Pending without first obtaining a completion key",
                        )).await);
                    }
                };
            let pending = match announce_pending_park(&completion_context, pending).await {
                Ok(pending) => pending,
                Err(failure) => {
                    return attempt_done(
                        normalized_outcome(context, &ids, tool_name, args, failure).await,
                    );
                }
            };
            return crate::ToolAttemptLaunch::Pending {
                key: Box::new(key),
                pending,
            };
        }
    };

    let result = finalize_tool_result_with_execution_context(
        context,
        &prepared.call_id,
        &tool_name,
        &args,
        result,
        duration_ms,
    )
    .await;

    let mut outcome = normalized_outcome(context, &ids, tool_name, args, result).await;
    outcome.intents = intents;
    attempt_done(outcome)
}

/// Executes one atomic tool attempt and reports everything it produced.
///
/// The caller installs a fresh `checkpoint_messages` buffer and a per-attempt
/// usage sink on `context` before calling, so the `ToolAttemptCapture` journaled
/// with this outcome carries exactly what *this* attempt committed — never a
/// prefix of a shared buffer a sibling may still be writing into. Consuming the
/// outcome restores those facts into the caller's own buffers, which is what
/// makes a journaled replay equivalent to a live execution.
pub async fn execute_prepared_tool_attempt_effect<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: PreparedToolCall,
    execution_grant: Option<Box<crate::ToolExecutionGrant>>,
    attempt: u32,
    max_attempts: u32,
    tool_context: ToolContext<'run>,
) -> Result<crate::ToolAttemptEffectOutcome, crate::RuntimeEffectControllerError> {
    let ids = ToolCallIds::of(&prepared);
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
        crate::ToolAttemptLaunch::Done {
            mut record,
            intents,
        } => {
            record.call_id = ids.call_id;
            record.provider_call_id = ids.provider_call_id;
            crate::ToolAttemptLaunch::Done { record, intents }
        }
        pending @ crate::ToolAttemptLaunch::Pending { .. } => pending,
    };
    let triggers = context.trigger_outcomes.drain();
    let capture = crate::runtime::ToolAttemptCapture {
        version: crate::runtime::TOOL_ATTEMPT_CAPTURE_VERSION,
        messages: context.checkpoint_messages.drain(),
        usage: context
            .direct_completions
            .usage_ledger()
            .map(|ledger| ledger.take())
            .unwrap_or_default(),
    };
    Ok(crate::ToolAttemptEffectOutcome {
        launch,
        triggers,
        capture,
    })
}

pub async fn finalize_tool_result_with_execution_context(
    context: &ToolDispatchContext<'_>,
    call_id: &lash_sansio::ToolCallId,
    tool_name: &str,
    args: &serde_json::Value,
    result: ToolOutcome,
    duration_ms: u64,
) -> ToolOutcome {
    match context
        .plugins
        .after_tool_call(ToolResultHookContext::new(
            context.session_id.clone(),
            call_id.clone(),
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

#[cfg(any(test, feature = "testing"))]
async fn tool_call_launch_into_done_or_runtime_failure(
    context: &ToolDispatchContext<'_>,
    ids: &ToolCallIds,
    launch: ToolCallLaunch,
) -> ToolDispatchOutcome {
    match launch {
        ToolCallLaunch::Done(outcome) => *outcome,
        ToolCallLaunch::Pending(pending) => {
            normalized_outcome(
                context,
                ids,
                pending.tool_name,
                pending.args,
                runtime_failure(
                    ToolFailureClass::Internal,
                    "pending_tool_not_supported_here",
                    "pending tool completion is not supported on this dispatch path",
                ),
            )
            .await
        }
        ToolCallLaunch::ControllerAborted(error) => {
            normalized_outcome(
                context,
                ids,
                "runtime_effect_controller".to_string(),
                serde_json::Value::Null,
                runtime_failure(
                    ToolFailureClass::Internal,
                    error.code.as_str(),
                    error.message,
                ),
            )
            .await
        }
    }
}
