use crate::{
    PreparedToolCall, ToolCallOutcome, ToolContext, ToolOutcome, ToolRetryPolicy, ToolRetryStatus,
};
use futures_util::FutureExt as _;
use lash_sansio::core_support::*;

use super::context::ToolDispatchContext;
use super::execution::AttemptAuthority;

pub(crate) fn resolve_retry_policy(
    context: &ToolDispatchContext<'_>,
    tool_id: &crate::ToolId,
    execution_grant: Option<&crate::ToolExecutionGrant>,
) -> ToolRetryPolicy {
    execution_grant
        .map(|grant| grant.manifest().retry_policy)
        .or_else(|| {
            super::preparation::resolve_callable_manifest_by_id(context, tool_id)
                .map(|manifest| manifest.retry_policy)
        })
        .unwrap_or(ToolRetryPolicy::Never)
}

/// Runs one numbered attempt of a leaf tool under its resolved authority.
///
/// The retry context is stamped with the authority's manifest name, so a
/// granted attempt reports the granted tool the same way a catalog attempt
/// reports its catalog member.
pub(super) async fn execute_leaf_tool_attempt<'run>(
    context: &ToolDispatchContext<'run>,
    authority: &AttemptAuthority<'_>,
    prepared: &PreparedToolCall,
    tool_context: ToolContext<'run>,
    attempt: u32,
    max_attempts: u32,
) -> crate::ToolAttemptOutcome {
    let tool_name = authority.manifest().name.as_str();
    execute_once(
        context,
        prepared,
        tool_context.with_retry_context(tool_name, attempt, max_attempts),
        authority.grant(),
    )
    .await
}

/// Runs a leaf tool body exactly once, with no retry ladder around it.
///
/// The grant selects the execution seam and the attachment producer name;
/// everything else — the attempt context, panic containment, attachment
/// normalization — is identical on both routes.
pub(crate) async fn execute_once<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: &PreparedToolCall,
    tool_context: ToolContext<'run>,
    grant: Option<&crate::ToolExecutionGrant>,
) -> crate::ToolAttemptOutcome {
    let mut attempt_result = match build_attempt_context(context, prepared, &tool_context).await {
        Ok(attempt_context) => {
            execute_attempt_body(context, prepared, grant, &attempt_context).await
        }
        Err(result) => crate::ToolAttemptOutcome::from_tool_result(result),
    };
    // A granted attempt is keyed on the grant name, never on whatever name the
    // provider's prepared call carries: the grant is the authority that
    // admitted the call, so it is the producer the attachment policy judges.
    let producer_name = match grant {
        Some(grant) => grant.manifest().name.as_str(),
        None => prepared.tool_name.as_str(),
    };
    normalize_attempt_result_attachments(context, producer_name, &mut attempt_result).await;
    attempt_result
}

async fn execute_attempt_body(
    context: &ToolDispatchContext<'_>,
    prepared: &PreparedToolCall,
    grant: Option<&crate::ToolExecutionGrant>,
    attempt_context: &crate::AttemptContext<'_>,
) -> crate::ToolAttemptOutcome {
    let body = async {
        match grant {
            Some(grant) => {
                context
                    .tools
                    .execute_granted_attempt(grant, &prepared.args, attempt_context)
                    .await
            }
            None => {
                context
                    .tools
                    .execute_attempt_by_id(&prepared.tool_id, &prepared.args, attempt_context)
                    .await
            }
        }
    };
    std::panic::AssertUnwindSafe(body)
        .catch_unwind()
        .await
        .unwrap_or_else(|payload| {
            crate::ToolAttemptOutcome::from_tool_result(tool_panicked(payload))
        })
}

async fn build_attempt_context<'run>(
    context: &ToolDispatchContext<'_>,
    prepared: &PreparedToolCall,
    tool_context: &ToolContext<'run>,
) -> Result<crate::AttemptContext<'run>, ToolOutcome> {
    let scoped = tool_context.effect_controller.scoped();
    let completion_key = tool_context.completion.load();
    // The key is reserved before the body runs, and only for a declared
    // deferrer on a controller that can route await events across process
    // loss. Report which of the two is missing rather than blaming the
    // controller for a provider that never declared the capability.
    let completion_support = if completion_key.is_some() {
        crate::tool_provider::AttemptCompletionSupport::Available
    } else if !context.tools.attempt_may_defer(&prepared.tool_id) {
        crate::tool_provider::AttemptCompletionSupport::NotDeclared
    } else {
        crate::tool_provider::AttemptCompletionSupport::ControllerUnsupported
    };
    Ok(crate::AttemptContext::from_tool_context(
        tool_context,
        scoped.scope_id().to_string(),
        completion_key,
        completion_support,
    ))
}

fn tool_panicked(payload: Box<dyn std::any::Any + Send>) -> ToolOutcome {
    let message = crate::panic_containment::payload_message(payload.as_ref());
    let failure = ToolOutcome::failure(crate::ToolFailure {
        class: crate::ToolFailureClass::Internal,
        code: "tool_panicked".to_string(),
        message,
        source: crate::ToolFailureSource::Runtime,
        retry: crate::ToolRetryStatus::Never,
        raw: None,
    });
    crate::panic_containment::enforce_loudness(payload);
    failure
}

async fn normalize_tool_result_attachments(
    context: &ToolDispatchContext<'_>,
    tool_name: &str,
    result: &mut ToolOutcome,
) {
    let Some(output) = result.as_done_output() else {
        return;
    };
    let sources = output.attachments();
    for source in sources {
        let producer = crate::AttachmentProducer::Tool {
            tool_name: tool_name.to_string(),
        };
        if let Err(error) = context
            .attachment_source_policy
            .authorize(&producer, &source)
        {
            *result = attachment_failure("attachment_source_policy_denied", error);
            return;
        }
        let crate::AttachmentSource::Inline { media_type, bytes } = &source else {
            continue;
        };
        let attachment_ref = match context
            .attachment_store
            .put(
                bytes.clone(),
                crate::AttachmentCreateMeta::new(media_type.clone(), None, None),
            )
            .await
        {
            Ok(attachment_ref) => attachment_ref,
            Err(error) => {
                *result = attachment_failure("attachment_store_failed", error);
                return;
            }
        };
        if let Some(output) = result.as_done_output().cloned() {
            let mut output = output;
            output.replace_attachment_source(
                &source,
                &crate::AttachmentSource::stored(attachment_ref),
            );
            *result = ToolOutcome::from_output(output);
        }
    }
}

async fn normalize_attempt_result_attachments(
    context: &ToolDispatchContext<'_>,
    tool_name: &str,
    result: &mut crate::ToolAttemptOutcome,
) {
    let crate::ToolAttemptOutcome::Done { result, .. } = result else {
        return;
    };
    let mut tool_result = ToolOutcome::from_output(result.clone().into_output());
    normalize_tool_result_attachments(context, tool_name, &mut tool_result).await;
    if let ToolOutcome::Done(output) = tool_result {
        *result = crate::ToolOutcomeDone::from_output(*output);
    }
}

fn attachment_failure(code: &str, error: impl std::fmt::Display) -> ToolOutcome {
    ToolOutcome::failure(crate::ToolFailure {
        class: crate::ToolFailureClass::Execution,
        code: code.to_string(),
        message: error.to_string(),
        source: crate::ToolFailureSource::Runtime,
        retry: crate::ToolRetryStatus::Never,
        raw: None,
    })
}

pub(crate) fn retry_after_ms(
    result: &ToolOutcome,
    retry_policy: ToolRetryPolicy,
    retry_index: u32,
) -> Option<u64> {
    if matches!(retry_policy, ToolRetryPolicy::Never) {
        return None;
    }
    let output = result.as_done_output()?;
    let ToolCallOutcome::Failure(failure) = &output.outcome else {
        return None;
    };
    let ToolRetryStatus::Safe { after_ms } = &failure.retry else {
        return None;
    };
    Some(retry_policy.delay_ms_for_retry(retry_index, *after_ms))
}

pub(crate) fn mark_retry_exhausted(result: ToolOutcome, attempts: u32) -> ToolOutcome {
    let mut output = match result.into_done_output() {
        Ok(output) => output,
        Err(pending) => return ToolOutcome::pending(pending),
    };
    if let ToolCallOutcome::Failure(failure) = &mut output.outcome {
        failure.retry = ToolRetryStatus::Exhausted { attempts };
    }
    ToolOutcome::from_output(output)
}

#[cfg(test)]
mod panic_tests {
    #[test]
    fn contained_tool_panic_is_loud_in_test_builds() {
        let previous = crate::panic_containment::set_loud(true);
        let panic = std::panic::catch_unwind(|| {
            let _ = super::tool_panicked(Box::new("tool seam remains loud"));
        });
        crate::panic_containment::set_loud(previous);
        assert!(panic.is_err());
    }
}
