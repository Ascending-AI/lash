use crate::{
    PreparedToolCall, ToolCallOutcome, ToolContext, ToolOutcome, ToolRetryPolicy, ToolRetryStatus,
};
use futures_util::FutureExt as _;
use lash_sansio::core_support::*;

use super::context::{ToolDispatchContext, ToolDispatchOutcome};
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
    match build_attempt_context(context, prepared, &tool_context, authority.grant()).await {
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
    .unwrap_or_else(|payload| tool_panicked(payload).into())
}

async fn build_attempt_context<'run>(
    context: &ToolDispatchContext<'_>,
    prepared: &PreparedToolCall,
    tool_context: &ToolContext<'run>,
    grant: Option<&crate::ToolExecutionGrant>,
) -> Result<crate::AttemptContext<'run>, ToolOutcome> {
    let scoped = tool_context.effect_controller.clone();
    // The key is reserved before the body runs, and only for a declared
    // deferrer on a controller that can route await events across process
    // loss. Report which of the two is missing rather than blaming the
    // controller for a provider that never declared the capability.
    let completion = match tool_context.completion.load() {
        Some(key) => crate::tool_provider::AttemptCompletionSupport::Available(key),
        None if !context.attempt_may_defer(&prepared.tool_id, grant) => {
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

/// A completed tool output that has crossed the attachment-policy and storage boundary.
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
    context: &ToolDispatchContext<'_>,
    ids: &super::context::ToolCallIds,
    tool_name: String,
    args: serde_json::Value,
    result: ToolOutcome,
) -> ToolDispatchOutcome {
    let output = Box::pin(normalize_tool_result_attachments(
        context, &tool_name, result,
    ))
    .await;
    super::context::outcome(ids, tool_name, args, output)
}

async fn normalize_tool_result_attachments(
    context: &ToolDispatchContext<'_>,
    tool_name: &str,
    result: ToolOutcome,
) -> NormalizedToolOutput {
    let mut output = result.into_done_output().unwrap_or_else(|_| {
        crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
            crate::ToolFailureClass::Internal,
            "pending_tool_not_finalized",
            "pending tool result reached a completed-output projection path",
        ))
    });
    let sources = output.attachments();
    let producer = crate::AttachmentProducer::Tool {
        tool_name: tool_name.to_string(),
    };
    for source in &sources {
        if let Err(error) = context
            .attachment_source_policy
            .authorize(&producer, source)
        {
            return NormalizedToolOutput(attachment_failure(
                "attachment_source_policy_denied",
                error,
            ));
        }
    }
    for source in sources {
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
                return NormalizedToolOutput(attachment_failure("attachment_store_failed", error));
            }
        };
        output.replace_attachment_source(&source, &crate::AttachmentSource::stored(attachment_ref));
    }
    NormalizedToolOutput(output)
}

fn attachment_failure(code: &str, error: impl std::fmt::Display) -> crate::ToolCallOutput {
    crate::ToolCallOutput::failure(crate::ToolFailure {
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
/// `call_id` is the parked call's durable identity, re-derived by the caller
/// from the journaled park rather than read out of it — the pending row does
/// not carry it — so the after-tool observation still names the call it
/// settles.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn settle_completed_pending_tool_call(
    context: &ToolDispatchContext<'_>,
    ids: &super::context::ToolCallIds,
    tool_name: String,
    args: serde_json::Value,
    resolution: crate::Resolution,
    resolver: Option<&crate::PendingResolver>,
    duration_ms: u64,
    attempts: Vec<lash_trace::TraceRetryAttempt>,
) -> ToolDispatchOutcome {
    let output = crate::tool_result::tool_output_from_completion_resolution(resolution, resolver);
    let result = super::finalize_tool_result_with_execution_context(
        context,
        &ids.call_id,
        &tool_name,
        &args,
        ToolOutcome::from_output(output),
        duration_ms,
    )
    .await;
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

#[cfg(test)]
mod panic_tests {
    use std::sync::Arc;

    use lash_sansio::sync::MutexExt as _;

    static PANIC_MODE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn process_await_preserves_plugin_retry_evidence_for_retry_policy() {
        let output = crate::ToolCallOutput::failure(crate::ToolFailure {
            class: crate::ToolFailureClass::External,
            code: "plugin_temporarily_unavailable".to_string(),
            message: "plugin asks the caller to retry".to_string(),
            source: crate::ToolFailureSource::Plugin,
            retry: crate::ToolRetryStatus::Safe { after_ms: Some(37) },
            raw: None,
        });
        let persisted = serde_json::to_value(crate::ProcessAwaitOutput::from_tool_output(output))
            .expect("serialize process await payload");
        let restored: crate::ProcessAwaitOutput =
            serde_json::from_value(persisted).expect("deserialize process await payload");
        let restored = crate::ToolOutcome::from_output(restored.into_tool_output());

        let failure = restored
            .as_done_output()
            .and_then(|output| match &output.outcome {
                crate::ToolCallOutcome::Failure(failure) => Some(failure),
                crate::ToolCallOutcome::Success(_) | crate::ToolCallOutcome::Cancelled(_) => None,
            })
            .expect("process await returns the plugin failure");
        assert_eq!(failure.source, crate::ToolFailureSource::Plugin);
        assert_eq!(
            failure.retry,
            crate::ToolRetryStatus::Safe { after_ms: Some(37) }
        );
        assert_eq!(
            super::retry_after_ms(
                &restored,
                crate::ToolRetryPolicy::Safe {
                    max_attempts: 2,
                    base_delay_ms: 1,
                    max_delay_ms: 100,
                },
                0,
            ),
            Some(37),
            "the retry layer must honor the plugin's preserved backoff hint"
        );
    }

    #[test]
    fn contained_tool_panic_is_loud_in_test_builds() {
        let _mode = PANIC_MODE.lock_recover();
        let previous = crate::panic_containment::set_loud(true);
        let panic = std::panic::catch_unwind(|| {
            let _ = super::tool_panicked(Box::new("tool seam remains loud"));
        });
        crate::panic_containment::set_loud(previous);
        assert!(panic.is_err());
    }

    /// A `ToolProvider` whose `execute` is written the way `async_trait`
    /// desugars the trait: its body runs when dispatch invokes the method, so
    /// this panic happens while the call's boxed future is being constructed —
    /// before any future exists for the unwind catcher's poll to cover.
    struct ConstructionPanicTool;

    fn construction_panic_tool_definition() -> crate::ToolDefinition {
        crate::ToolDefinition::raw(
            "tool:construction_panic_tool",
            "construction_panic_tool",
            "panics before returning its execute future",
            crate::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "object" }),
        )
    }

    impl crate::ToolProvider for ConstructionPanicTool {
        fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
            vec![construction_panic_tool_definition().manifest()]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
            (name == "construction_panic_tool")
                .then(|| Arc::new(construction_panic_tool_definition().contract()))
        }

        fn execute<'life0, 'life1, 'async_trait>(
            &'life0 self,
            _call: crate::ToolCall<'life1>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::ToolAttemptOutcome> + Send + 'async_trait>,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait,
        {
            panic!("tool construction payload")
        }
    }

    fn construction_panic_dispatch() -> (
        Arc<super::ToolDispatchContext<'static>>,
        crate::PreparedToolCall,
    ) {
        let definition = construction_panic_tool_definition();
        let prepared = crate::PreparedToolCall {
            call_id: crate::ToolCallId::fixture("construction-panic-call"),
            provider_call_id: None,
            tool_id: definition.manifest().id.clone(),
            tool_name: "construction_panic_tool".to_string(),
            args: serde_json::json!({}),
            replay: None,
            prepared_payload: serde_json::Value::Null,
        };
        let plugins = crate::plugin::PluginHost::empty()
            .build_session(crate::plugin::PluginSessionRequest::creation(
                "session",
                Default::default(),
            ))
            .expect("plugin session");
        let dispatch = Arc::new(super::ToolDispatchContext {
            plugins,
            tools: Arc::new(ConstructionPanicTool),
            tool_registry: None,
            tool_catalog: Arc::new(crate::ToolCatalog::from_tool_definitions(vec![definition])),
            sessions: Arc::new(crate::testing::MockSessionManager::default()),
            session_lifecycle: Arc::new(crate::testing::MockSessionManager::default()),
            session_graph: Arc::new(crate::testing::MockSessionManager::default()),
            processes: Arc::new(crate::UnavailableProcessService),
            trigger_router: None,
            process_definitions: None,
            process_engines: crate::ProcessEngineRegistry::default(),
            effect_controller: crate::runtime::ScopedEffectController::shared(
                Arc::new(crate::testing::UnavailableEffectController),
                crate::AdmittedScope::runtime_operation("test-runtime-effect-controller"),
            )
            .expect("valid test runtime scope"),
            direct_completions: crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
            parent_invocation: None,
            observation_call_key: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            ),
            owner: crate::ExecutionOwner::SessionFrame {
                session_id: crate::SessionId::from("session"),
                agent_frame_id: crate::FrameNodeId::new("test-frame").unwrap(),
            },
            observer: Arc::new(crate::engine::NullObservationSink),
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::new(crate::RuntimeAttachmentStore::unavailable()),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: crate::TurnContext::default(),
            clock: Arc::new(crate::SystemClock),
            process_lineage: None,
            process_originator: None,
        });
        (dispatch, prepared)
    }

    #[test]
    fn tool_execute_construction_panic_is_typed_in_quiet_and_loud_modes() {
        use futures_util::FutureExt as _;

        let _mode = PANIC_MODE.lock_recover();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("construction panic test runtime");
        for loud in [false, true] {
            let previous = crate::panic_containment::set_loud(loud);
            let (dispatch, prepared) = construction_panic_dispatch();
            let tool_context =
                crate::testing::ToolCallFixture::from_dispatch(Arc::clone(&dispatch))
                    .prepared_call(&prepared)
                    .context;
            let direct = runtime.block_on(
                std::panic::AssertUnwindSafe(super::execute_once(
                    &dispatch,
                    &prepared,
                    tool_context,
                    None,
                ))
                .catch_unwind(),
            );
            if loud {
                let payload = direct.expect_err("loud construction panic propagates");
                assert_eq!(
                    payload.downcast_ref::<&str>(),
                    Some(&"tool construction payload")
                );
            } else {
                let crate::ToolAttemptOutcome::Done { result, .. } =
                    direct.expect("quiet containment does not unwind")
                else {
                    panic!("a contained construction panic resolves as a done outcome")
                };
                let output = result.into_output();
                let crate::ToolCallOutcome::Failure(failure) = output.outcome else {
                    panic!("tool construction panic is recorded as a failure")
                };
                assert_eq!(failure.class, crate::ToolFailureClass::Internal);
                assert_eq!(failure.code, "tool_panicked");
                assert_eq!(failure.message, "tool construction payload");
                assert_eq!(failure.retry, crate::ToolRetryStatus::Never);
            }
            crate::panic_containment::set_loud(previous);
        }
    }
}
