use crate::{
    PreparedToolCall, ToolCallOutcome, ToolContext, ToolManifest, ToolResult, ToolRetryDisposition,
    ToolRetryPolicy,
};
use lash_sansio::core_support::*;

use super::context::ToolDispatchContext;

pub(super) async fn execute_tool_attempt<'run>(
    context: &ToolDispatchContext<'run>,
    manifest: &ToolManifest,
    prepared: &PreparedToolCall,
    tool_context: ToolContext<'run>,
    attempt: u32,
    max_attempts: u32,
) -> ToolResult {
    let tool_name = manifest.name.as_str();
    execute_once(
        context,
        prepared,
        tool_context.with_retry_context(tool_name, attempt, max_attempts),
    )
    .await
}

pub(super) async fn execute_granted_tool_attempt<'run>(
    context: &ToolDispatchContext<'run>,
    grant: &crate::ToolExecutionGrant,
    prepared: &PreparedToolCall,
    tool_context: ToolContext<'run>,
    attempt: u32,
    max_attempts: u32,
) -> ToolResult {
    let tool_name = grant.manifest.name.as_str();
    execute_granted_once(
        context,
        grant,
        prepared,
        tool_context.with_retry_context(tool_name, attempt, max_attempts),
    )
    .await
}

async fn execute_once<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: &PreparedToolCall,
    tool_context: ToolContext<'run>,
) -> ToolResult {
    let args = &prepared.args;
    let mut result = context
        .tools
        .execute_by_id(&prepared.tool_id, args, &tool_context)
        .await;
    normalize_tool_result_attachments(context, &prepared.tool_name, &mut result).await;
    result
}

async fn execute_granted_once<'run>(
    context: &ToolDispatchContext<'run>,
    grant: &crate::ToolExecutionGrant,
    prepared: &PreparedToolCall,
    tool_context: ToolContext<'run>,
) -> ToolResult {
    let mut result = context
        .tools
        .execute_granted(grant, &prepared.args, &tool_context)
        .await;
    normalize_tool_result_attachments(context, &grant.manifest.name, &mut result).await;
    result
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
