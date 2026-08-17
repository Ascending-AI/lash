use crate::{
    PreparedToolCall, ToolCallOutcome, ToolContext, ToolManifest, ToolResult, ToolRetryDisposition,
    ToolRetryPolicy,
};
use futures_util::FutureExt as _;
use lash_sansio::core_support::*;

use super::context::ToolDispatchContext;

pub(super) async fn execute_leaf_tool_attempt<'run>(
    context: &ToolDispatchContext<'run>,
    manifest: &ToolManifest,
    prepared: &PreparedToolCall,
    tool_context: ToolContext<'run>,
    attempt: u32,
    max_attempts: u32,
) -> crate::ToolAttemptResult {
    let tool_name = manifest.name.as_str();
    execute_once(
        context,
        prepared,
        tool_context.with_retry_context(tool_name, attempt, max_attempts),
    )
    .await
}

pub(super) async fn execute_granted_leaf_tool_attempt<'run>(
    context: &ToolDispatchContext<'run>,
    grant: &crate::ToolExecutionGrant,
    prepared: &PreparedToolCall,
    tool_context: ToolContext<'run>,
    attempt: u32,
    max_attempts: u32,
) -> crate::ToolAttemptResult {
    let tool_name = grant.manifest.name.as_str();
    execute_granted_once(
        context,
        grant,
        prepared,
        tool_context.with_retry_context(tool_name, attempt, max_attempts),
    )
    .await
}

pub(crate) async fn execute_once<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: &PreparedToolCall,
    tool_context: ToolContext<'run>,
) -> crate::ToolAttemptResult {
    let mut attempt_result = execute_with_attempt_context(context, prepared, &tool_context).await;
    normalize_attempt_result_attachments(context, &prepared.tool_name, &mut attempt_result).await;
    attempt_result
}

async fn execute_granted_once<'run>(
    context: &ToolDispatchContext<'run>,
    grant: &crate::ToolExecutionGrant,
    prepared: &PreparedToolCall,
    tool_context: ToolContext<'run>,
) -> crate::ToolAttemptResult {
    let mut attempt_result = match build_attempt_context(context, prepared, &tool_context).await {
        Ok(attempt_context) => std::panic::AssertUnwindSafe(context.tools.execute_granted_attempt(
            grant,
            &prepared.args,
            &attempt_context,
        ))
        .catch_unwind()
        .await
        .unwrap_or_else(|payload| {
            crate::ToolAttemptResult::from_tool_result(tool_panicked(payload))
        }),
        Err(result) => crate::ToolAttemptResult::from_tool_result(result),
    };
    normalize_attempt_result_attachments(context, &grant.manifest.name, &mut attempt_result).await;
    attempt_result
}

async fn execute_with_attempt_context(
    context: &ToolDispatchContext<'_>,
    prepared: &PreparedToolCall,
    tool_context: &ToolContext<'_>,
) -> crate::ToolAttemptResult {
    let attempt_context = match build_attempt_context(context, prepared, tool_context).await {
        Ok(context) => context,
        Err(result) => return crate::ToolAttemptResult::from_tool_result(result),
    };
    std::panic::AssertUnwindSafe(context.tools.execute_attempt_by_id(
        &prepared.tool_id,
        &prepared.args,
        &attempt_context,
    ))
    .catch_unwind()
    .await
    .unwrap_or_else(|payload| crate::ToolAttemptResult::from_tool_result(tool_panicked(payload)))
}

async fn build_attempt_context<'run>(
    context: &ToolDispatchContext<'_>,
    prepared: &PreparedToolCall,
    tool_context: &ToolContext<'run>,
) -> Result<crate::AttemptContext<'run>, ToolResult> {
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

fn tool_panicked(payload: Box<dyn std::any::Any + Send>) -> ToolResult {
    let message = crate::panic_containment::payload_message(payload.as_ref());
    let failure = ToolResult::failure(crate::ToolFailure {
        class: crate::ToolFailureClass::Internal,
        code: "tool_panicked".to_string(),
        message,
        source: crate::ToolFailureSource::Runtime,
        retry: crate::ToolRetryDisposition::Never,
        raw: None,
    });
    crate::panic_containment::enforce_loudness(payload);
    failure
}

async fn normalize_tool_result_attachments(
    context: &ToolDispatchContext<'_>,
    tool_name: &str,
    result: &mut ToolResult,
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
            *result = ToolResult::from_output(output);
        }
    }
}

async fn normalize_attempt_result_attachments(
    context: &ToolDispatchContext<'_>,
    tool_name: &str,
    result: &mut crate::ToolAttemptResult,
) {
    let crate::ToolAttemptResult::Done { result, .. } = result else {
        return;
    };
    let mut tool_result = ToolResult::from_output(result.clone().into_output());
    normalize_tool_result_attachments(context, tool_name, &mut tool_result).await;
    if let ToolResult::Done(output) = tool_result {
        *result = crate::ToolResultDone::from_output(*output);
    }
}

fn attachment_failure(code: &str, error: impl std::fmt::Display) -> ToolResult {
    ToolResult::failure(crate::ToolFailure {
        class: crate::ToolFailureClass::Execution,
        code: code.to_string(),
        message: error.to_string(),
        source: crate::ToolFailureSource::Runtime,
        retry: crate::ToolRetryDisposition::Never,
        raw: None,
    })
}

pub(crate) fn retry_after_ms(
    result: &ToolResult,
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
    let ToolRetryDisposition::Safe { after_ms } = &failure.retry else {
        return None;
    };
    Some(retry_policy.delay_ms_for_retry(retry_index, *after_ms))
}

pub(crate) fn mark_retry_exhausted(result: ToolResult, attempts: u32) -> ToolResult {
    let mut output = match result.into_done_output() {
        Ok(output) => output,
        Err(pending) => return ToolResult::pending(pending),
    };
    if let ToolCallOutcome::Failure(failure) = &mut output.outcome {
        failure.retry = ToolRetryDisposition::Exhausted { attempts };
    }
    ToolResult::from_output(output)
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
