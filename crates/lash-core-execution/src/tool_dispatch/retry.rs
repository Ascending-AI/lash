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
    .unwrap_or_else(|payload| tool_panicked(payload).into())
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

fn tool_panicked(payload: Box<dyn std::any::Any + Send>) -> ToolOutcome {
    let message = crate::panic_containment::payload_message(payload.as_ref());
    let failure = ToolOutcome::failure(crate::ToolFailure {
        cause: None,
        class: crate::ToolFailureClass::Internal,
        code: "tool_panicked".to_string(),
        message,
        source: crate::ToolFailureSource::Runtime,
        suggested_delay_ms: None,
        raw: None,
    });
    crate::panic_containment::enforce_loudness(payload);
    failure
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

#[cfg(test)]
mod panic_tests {
    use std::sync::Arc;

    use lash_sansio::sync::MutexExt as _;

    static PANIC_MODE: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
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
        let plugins = crate::plugin::PluginHost::empty(
            crate::ExecutionBudgets::recommended(),
            crate::trace::TraceRuntime::new(std::sync::Arc::new(crate::SystemClock)),
        )
        .build_session(crate::plugin::PluginSessionRequest::creation(
            "session",
            crate::plugin::SessionAuthorityContext::ambient_fixture(),
        ))
        .expect("plugin session");
        let dispatch = Arc::new(super::ToolDispatchContext {
            fleet_format: crate::FleetFormat::current(),
            plugins,
            tools: Arc::new(ConstructionPanicTool),
            tool_registry: None,
            tool_catalog: Arc::new(crate::ToolCatalog::from_tool_definitions(vec![definition])),
            sessions: Arc::new(crate::testing::MockSessionManager::default()),
            session_lifecycle: Arc::new(crate::testing::MockSessionManager::default()),
            session_graph: Arc::new(crate::testing::MockSessionManager::default()),
            processes: Arc::new(crate::UnavailableProcessService),
            process_engines: crate::ProcessEngineRegistry::default(),
            effect_controller: crate::ActorContext::unavailable()
                .scoped(crate::AdmittedScope::runtime_operation(
                    "test-runtime-effect-controller",
                ))
                .expect("valid test runtime scope"),
            direct_completions: crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
            parent_invocation: None,
            observation_call_key: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::AdmittedPluginConfig::default(),
                crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                    crate::NoProgressBudget::bounded(12),
                ),
                crate::SessionToolAccess::ambient(),
            ),
            owner: crate::ExecutionOwner::SessionFrame {
                session_id: crate::SessionId::from("session"),
                agent_frame_id: crate::FrameNodeId::new("test-frame").unwrap(),
            },
            observer: Arc::new(crate::engine::NullObservationSink),
            attachment_store: Arc::new(crate::RuntimeAttachmentStore::unavailable()),

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
                assert_eq!(failure.suggested_delay_ms, None);
            }
            crate::panic_containment::set_loud(previous);
        }
    }
}
