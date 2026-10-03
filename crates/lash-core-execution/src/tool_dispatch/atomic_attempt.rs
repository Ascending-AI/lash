//! Executes one prepared, opaque tool attempt without a child request or host.
//!
//! Coordination owns journal commands, retries and cancellation. This runner
//! owns validation, body execution and the captures returned to that journal.
//! Providers receive only the sealed `AttemptContext`: no recursive durable
//! dispatch, unmanaged process spawn or generic process administration.

use std::sync::Arc;

use super::context::{ToolCallIds, ToolDispatchContext, attempt_done, runtime_failure};
use super::retry::{execute_leaf_tool_attempt, normalized_outcome};
use crate::{PreparedToolCall, ToolContext, ToolFailureClass, ToolManifest, ToolOutcome};

/// Runtime-only execution of one already prepared attempt. Its consuming
/// entry point returns facts; it cannot issue a retry or drive a child.
pub(crate) struct AtomicToolAttempt<'run> {
    dispatch: Arc<ToolDispatchContext<'run>>,
    tool_context: ToolContext<'run>,
}

impl<'run> AtomicToolAttempt<'run> {
    pub(crate) fn new(
        context: &ToolDispatchContext<'run>,
        tool_context: ToolContext<'run>,
        invocation: crate::RuntimeInvocation,
        completion_key: Option<crate::AwaitEventKey>,
        effect_attempt: Option<crate::EffectAttempt>,
    ) -> Self {
        let mut dispatch = context.clone();
        dispatch.parent_invocation = Some(invocation.clone());
        dispatch.observation_call_key = None;
        dispatch.direct_completions = dispatch
            .direct_completions
            .with_tool_attempt_parent_invocation(invocation.clone())
            .with_effect_attempt(effect_attempt);
        dispatch.trigger_outcomes = super::ToolTriggerOutcomeBuffer::default();
        dispatch.checkpoint_messages = super::CheckpointMessageBuffer::default();
        let dispatch = Arc::new(dispatch);
        let mut tool_context =
            tool_context.with_attempt_dispatch(Arc::clone(&dispatch), invocation);
        tool_context.completion = crate::tool_provider::ToolCompletionState::default();
        tool_context.install_prederived_completion_key(completion_key);
        Self {
            dispatch,
            tool_context,
        }
    }

    /// Run only inside an unrecorded effect body. The effect host serves a
    /// recorded result without constructing or executing this runner.
    pub(crate) async fn execute(
        self,
        prepared: PreparedToolCall,
        execution_grant: Option<Box<crate::ToolExecutionGrant>>,
        attempt: u32,
        max_attempts: u32,
    ) -> Result<crate::ToolAttemptEffectOutcome, crate::RuntimeEffectControllerError> {
        if self.tool_context.call_id != prepared.call_id
            || self.tool_context.owner != self.dispatch.owner
        {
            return Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "tool attempt context does not belong to the prepared call and its execution owner",
            ));
        }
        let context = self.dispatch.as_ref();
        let tool_context = self.tool_context;
        let ids = ToolCallIds::of(&prepared);
        let launch = Box::pin(dispatch_prepared_tool_attempt_launch(
            context,
            execution_grant.as_deref(),
            prepared,
            attempt,
            max_attempts,
            tool_context,
        ))
        .await?;
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
        };
        Ok(crate::ToolAttemptEffectOutcome {
            launch,
            triggers,
            capture,
        })
    }
}

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

/// The failure an outcome its admitted declaration does not admit answers
/// with. Typed by its [`crate::DeclarationRefusal`], so a host can tell an
/// authoring defect from a tool's own failure.
fn declaration_refused(tool_name: &str, refusal: crate::DeclarationRefusal) -> ToolOutcome {
    ToolOutcome::failure(
        crate::ToolFailure::runtime(
            ToolFailureClass::Internal,
            "tool_outcome_not_declared",
            format!(
                "tool `{tool_name}` returned an outcome its declaration does not admit: {refusal}"
            ),
        )
        .with_cause(crate::ToolFailureCause::Declaration { refusal }),
    )
}

/// Launches one prepared tool attempt under whichever authority admitted it.
///
/// Catalog calls and granted calls share this body: the authority is resolved
/// once at entry, and the two routes differ only through
/// [`AttemptAuthority`]'s methods.
async fn dispatch_prepared_tool_attempt_launch<'run>(
    context: &ToolDispatchContext<'run>,
    grant: Option<&crate::ToolExecutionGrant>,
    prepared: PreparedToolCall,
    attempt: u32,
    max_attempts: u32,
    tool_context: ToolContext<'run>,
) -> Result<crate::ToolAttemptLaunch, crate::RuntimeEffectControllerError> {
    let args = prepared.args.clone();
    let ids = ToolCallIds::of(&prepared);
    let Some(authority) = AttemptAuthority::resolve(context, &prepared.tool_id, grant) else {
        return Ok(attempt_done(
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
        ));
    };
    let tool_name = authority.manifest().name.clone();
    let declaration = authority.manifest().declaration.clone();
    if let Err(failure) = authority.verify_prepared_identity(&prepared) {
        return Ok(attempt_done(
            normalized_outcome(context, &ids, tool_name, args, failure).await,
        ));
    }

    let tool_context = authority.apply_execution_binding(
        tool_context.with_prepared_payload(prepared.prepared_payload.clone()),
    );
    let completion_context = tool_context.clone();
    let attempt_result = Box::pin(execute_leaf_tool_attempt(
        context,
        &authority,
        &prepared,
        tool_context,
        attempt,
        max_attempts,
    ))
    .await;
    let (result, intents) = match attempt_result {
        crate::ToolAttemptOutcome::Done { result, intents } => {
            let kinds: Vec<_> = intents
                .intents
                .iter()
                .map(crate::ToolIntent::kind)
                .collect();
            match declaration.admits(crate::OutcomeShape::Done { intents: &kinds }) {
                Ok(()) => (ToolOutcome::from_output(result.into_output()), intents),
                // An undeclared intent is refused with the whole outcome, before
                // the attempt is recorded: nothing it declared is realized.
                Err(refusal) => (
                    declaration_refused(&tool_name, refusal),
                    crate::ToolIntents::default(),
                ),
            }
        }
        crate::ToolAttemptOutcome::HostFailed(error) => return Err(*error),
        crate::ToolAttemptOutcome::Pending(pending) => {
            // An undeclared Deferred never parks: no key was reserved for it,
            // and the refusal names the declaration rather than the key.
            if let Err(refusal) = declaration.admits(crate::OutcomeShape::Deferred) {
                let refused = declaration_refused(&tool_name, refusal);
                return Ok(attempt_done(
                    normalized_outcome(context, &ids, tool_name, args, refused).await,
                ));
            }
            let key =
                match completion_context.take_completion_key() {
                    Some(key) => key,
                    None => {
                        return Ok(attempt_done(normalized_outcome(
                        context,
                        &ids,
                        tool_name,
                        args,
                        runtime_failure(
                            ToolFailureClass::Internal,
                            "pending_tool_missing_completion_key",
                            "tool returned Pending without first obtaining a completion key",
                        )).await));
                    }
                };
            let pending = match announce_pending_park(&completion_context, pending).await {
                Ok(pending) => pending,
                Err(failure) => {
                    return Ok(attempt_done(
                        normalized_outcome(context, &ids, tool_name, args, failure).await,
                    ));
                }
            };
            return hold_declared_execution_environments(
                context,
                crate::ToolAttemptLaunch::Pending {
                    key: Box::new(key),
                    pending,
                },
                &completion_context.execution_env_spec,
            )
            .await;
        }
    };

    let result = super::finalize_tool_result_with_execution_context(
        context,
        &crate::plugin::PreparedCallReadView::new(prepared),
        super::attempt_occurrence(attempt),
        result,
    )
    .await;

    let mut outcome = normalized_outcome(context, &ids, tool_name, args, result).await;
    outcome.intents = intents;
    hold_declared_execution_environments(
        context,
        attempt_done(outcome),
        &completion_context.execution_env_spec,
    )
    .await
}

async fn hold_declared_execution_environments(
    context: &ToolDispatchContext<'_>,
    launch: crate::ToolAttemptLaunch,
    captured_spec: &crate::ProcessExecutionEnvSpec,
) -> Result<crate::ToolAttemptLaunch, crate::RuntimeEffectControllerError> {
    let mut env_refs: Vec<_> = match &launch {
        crate::ToolAttemptLaunch::Done { intents, .. }
            if super::intent_executor::admit_batch(&context.owner.runtime_owner(), intents)
                .is_none() =>
        {
            intents
                .intents
                .iter()
                .filter(|intent| match intent {
                    crate::ToolIntent::RegisterTrigger(registration) => {
                        super::intent_executor::validate_trigger_registration_authority(
                            context,
                            registration,
                        )
                        .is_none()
                    }
                    _ => true,
                })
                .filter_map(crate::ToolIntent::execution_env_ref)
                .cloned()
                .collect()
        }
        crate::ToolAttemptLaunch::Done { .. } => Vec::new(),
        crate::ToolAttemptLaunch::Pending { pending, .. } => match &pending.resolved_by {
            Some(crate::PendingResolver::DeclaredStart(start)) => {
                start.start().declaration.env_ref.iter().cloned().collect()
            }
            _ => Default::default(),
        },
    };
    env_refs.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
    env_refs.dedup();
    if !env_refs.is_empty() {
        let ports = context.process_engines.artifact_ports().ok_or_else(|| {
            crate::PluginError::Session(
                "an environment declaration requires the runtime's artifact stores".to_string(),
            )
        })?;
        let claim =
            crate::session::execution_claim_of(context.effect_controller.execution_scope())?;
        let captured_ref = captured_spec
            .stable_ref()
            .map_err(|error| crate::PluginError::Session(error.to_string()))?;
        for env_ref in env_refs {
            if env_ref == captured_ref {
                crate::publish_process_execution_env(ports.env().as_ref(), &claim, captured_spec)
                    .await?;
            } else {
                ports
                    .env()
                    .acquire_process_execution_env(&claim, &env_ref)
                    .await
                    .map_err(crate::PluginError::from)?;
            }
        }
    }
    Ok(launch)
}
