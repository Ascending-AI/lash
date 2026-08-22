
/// What a granted probe's body does when the dispatch path reaches it.
#[derive(Clone, Copy)]
enum GrantProbeMode {
    /// Takes its completion key and parks.
    PendingWithKey,
    /// Fails retryably on every attempt, so the retry ladder runs to
    /// exhaustion.
    AlwaysRetryable,
    /// Succeeds carrying an inline attachment the runtime has to normalize.
    InlineAttachment,
}

/// A provider reached through an execution grant, which records the execution
/// binding its body observed.
#[derive(Clone)]
struct GrantProbeTools {
    definition: crate::ToolDefinition,
    attempts: Arc<AtomicUsize>,
    mode: GrantProbeMode,
    observed_execution_bindings: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
}

#[async_trait::async_trait]
impl ToolProvider for GrantProbeTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        manifests(vec![self.definition.clone()])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == self.definition.name()).then(|| Arc::new(self.definition.contract()))
    }

    fn attempt_may_defer(&self, tool_id: &crate::ToolId) -> bool {
        tool_id == self.definition.id() && matches!(self.mode, GrantProbeMode::PendingWithKey)
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        self.observed_execution_bindings
            .lock_recover()
            .push(call.context.tool_execution_binding().clone());
        match self.mode {
            GrantProbeMode::PendingWithKey => {
                call.context.completion_key().expect("completion key");
                ToolOutcome::pending(crate::PendingCompletion::new())
            }
            GrantProbeMode::AlwaysRetryable => ToolOutcome::retryable_failure(
                crate::ToolFailureClass::External,
                "transient",
                "transient granted failure",
                Some(0),
            ),
            GrantProbeMode::InlineAttachment => {
                ToolOutcome::from_output(crate::ToolCallOutput::success(
                    crate::ToolValue::Attachment(crate::AttachmentSource::inline(
                        crate::MediaType::parse("text/plain").expect("media type"),
                        b"granted attachment bytes".to_vec(),
                    )),
                ))
            }
        }
    }
}

/// Records every producer the attachment source policy is asked to authorize.
struct RecordingAttachmentSourcePolicy {
    producers: Arc<std::sync::Mutex<Vec<crate::AttachmentProducer>>>,
}

impl crate::AttachmentSourcePolicy for RecordingAttachmentSourcePolicy {
    fn authorize(
        &self,
        producer: &crate::AttachmentProducer,
        _source: &crate::AttachmentSource,
    ) -> Result<(), crate::test_support::AttachmentSourcePolicyError> {
        self.producers.lock_recover().push(producer.clone());
        Ok(())
    }
}

fn grant_probe_tool(retry_policy: ToolRetryPolicy) -> crate::ToolDefinition {
    named_beta_tool("grant_probe").with_retry_policy(retry_policy)
}

/// Builds a dispatch context whose only provider is a granted probe, plus the
/// grant that authorizes it.
fn grant_probe_dispatch(
    mode: GrantProbeMode,
    attempts: Arc<AtomicUsize>,
    retry_policy: ToolRetryPolicy,
    observed_execution_bindings: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
) -> (ToolDispatchContext<'static>, crate::ToolExecutionGrant) {
    let definition = grant_probe_tool(retry_policy);
    let provider: Arc<dyn ToolProvider> = Arc::new(GrantProbeTools {
        definition: definition.clone(),
        attempts,
        mode,
        observed_execution_bindings,
    });
    let grant = crate::ToolExecutionGrant::from_definition(definition)
        .with_source_id(crate::PLUGIN_TOOL_SOURCE_ID)
        .with_execution_binding(json!({ "kind": "grant-probe" }));
    (exact_dispatch_context(provider), grant)
}

/// Builds a prepared granted call. `tool_name` is the name the provider stamped
/// on the prepared call, which may differ from the grant's tool name.
fn grant_prepared_call(tool_name: &str) -> crate::PreparedToolCall {
    grant_prepared_call_with_id("tool:grant_probe", tool_name)
}

/// Builds a prepared granted call with an explicit tool id, so a test can hand
/// dispatch a call whose identifier disagrees with the grant's manifest.
fn grant_prepared_call_with_id(tool_id: &str, tool_name: &str) -> crate::PreparedToolCall {
    crate::PreparedToolCall::from_parts(
        "grant-call",
        tool_id,
        tool_name,
        json!({ "value": "ok" }),
        None,
        serde_json::Value::Null,
    )
}

/// A granted attempt parks exactly like a catalog attempt: the completion key
/// is taken and the launch is handed back pending under the grant's tool name.
/// The grant's execution binding reaches the executing body, so a granted tool
/// runs bound even on the path that parks.
#[tokio::test]
async fn granted_pending_park_returns_a_pending_launch_under_the_grant_binding() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed_execution_bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (context, grant) = grant_probe_dispatch(
        GrantProbeMode::PendingWithKey,
        Arc::clone(&attempts),
        ToolRetryPolicy::safe(5, 0, 0),
        Arc::clone(&observed_execution_bindings),
    );
    let prepared = grant_prepared_call("grant_probe");
    let tool_context = tool_context_for_prepared(&context, &prepared);

    let launch = coordinate_prepared_tool_call_launch_with_execution_context(
        &context,
        prepared,
        Some(Box::new(grant)),
        tool_context,
    )
    .await;

    let ToolCallLaunch::Pending(pending) = launch else {
        panic!("a granted tool that parks must launch pending");
    };
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(pending.tool_name, "grant_probe");
    assert_eq!(
        pending.key.wait,
        crate::AwaitEventWaitIdentity::tool_completion("grant-call")
    );
    assert_eq!(
        *observed_execution_bindings.lock_recover(),
        vec![json!({ "kind": "grant-probe" })],
        "the grant binding must reach the parking body"
    );
}

/// A granted attempt runs the same retry ladder as a catalog attempt, and the
/// final failure is marked exhausted rather than left retryable. The ladder is
/// bounded by the *grant's* manifest policy, which is the only retry policy a
/// non-catalog tool has.
#[tokio::test]
async fn granted_retry_ladder_marks_exhausted_after_the_final_attempt() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed_execution_bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (context, grant) = grant_probe_dispatch(
        GrantProbeMode::AlwaysRetryable,
        Arc::clone(&attempts),
        ToolRetryPolicy::safe(2, 0, 0),
        Arc::clone(&observed_execution_bindings),
    );
    let prepared = grant_prepared_call("grant_probe");
    let tool_context = tool_context_for_prepared(&context, &prepared);

    let launch = coordinate_prepared_tool_call_launch_with_execution_context(
        &context,
        prepared,
        Some(Box::new(grant)),
        tool_context,
    )
    .await;

    let ToolCallLaunch::Done(outcome) = launch else {
        panic!("an exhausted granted ladder must complete, not park");
    };
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(outcome.record.tool, "grant_probe");
    let ToolCallOutcome::Failure(failure) = outcome.record.output.outcome else {
        panic!("expected an exhausted failure");
    };
    assert_eq!(failure.retry, ToolRetryStatus::Exhausted { attempts: 2 });
}

/// The attachment producer of a granted attempt is the grant's tool name, not
/// whatever name the provider stamped on the prepared call. The grant is the
/// authority that admitted the call, so it is the producer the attachment
/// source policy judges — a provider that renames its prepared call must not be
/// able to move itself to a different producer identity.
#[tokio::test]
async fn granted_attachment_producer_is_the_grant_name_when_the_prepared_call_is_renamed() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed_execution_bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
    let producers = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (mut context, grant) = grant_probe_dispatch(
        GrantProbeMode::InlineAttachment,
        Arc::clone(&attempts),
        ToolRetryPolicy::Never,
        Arc::clone(&observed_execution_bindings),
    );
    context.attachment_source_policy = Arc::new(RecordingAttachmentSourcePolicy {
        producers: Arc::clone(&producers),
    });
    let prepared = grant_prepared_call("renamed_by_the_provider");
    let tool_context = tool_context_for_prepared(&context, &prepared);

    let launch = coordinate_prepared_tool_call_launch_with_execution_context(
        &context,
        prepared,
        Some(Box::new(grant)),
        tool_context,
    )
    .await;

    let ToolCallLaunch::Done(outcome) = launch else {
        panic!("the granted attachment call must complete");
    };
    assert!(outcome.record.output.is_success());
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        *producers.lock_recover(),
        vec![crate::AttachmentProducer::Tool {
            tool_name: "grant_probe".to_string(),
        }],
    );
}

/// A prepared call whose tool id disagrees with the grant's manifest id is
/// refused before the tool body runs. This is the one guard that is grant-only
/// by decision rather than by construction: under a grant the manifest comes
/// from the grant and the prepared call comes from the provider, so the pair
/// really can drift, and a drifted pair must never reach execution under the
/// grant's authority.
#[tokio::test]
async fn granted_identifier_mismatch_is_refused_before_the_tool_body_runs() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed_execution_bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (context, grant) = grant_probe_dispatch(
        GrantProbeMode::InlineAttachment,
        Arc::clone(&attempts),
        ToolRetryPolicy::Never,
        Arc::clone(&observed_execution_bindings),
    );
    let prepared = grant_prepared_call_with_id("tool:not_the_granted_id", "grant_probe");
    let tool_context = tool_context_for_prepared(&context, &prepared);

    let launch = coordinate_prepared_tool_call_launch_with_execution_context(
        &context,
        prepared,
        Some(Box::new(grant)),
        tool_context,
    )
    .await;

    let ToolCallLaunch::Done(outcome) = launch else {
        panic!("a mismatched granted identifier must fail the launch synchronously");
    };
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        0,
        "the tool body must never run under a mismatched identifier"
    );
    assert_eq!(outcome.record.tool, "grant_probe");
    let ToolCallOutcome::Failure(failure) = &outcome.record.output.outcome else {
        panic!("expected a refusal failure");
    };
    assert_eq!(failure.code, "granted_tool_id_mismatch");
    assert_eq!(failure.class, crate::ToolFailureClass::Internal);
}
