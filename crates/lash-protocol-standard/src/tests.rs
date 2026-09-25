use super::*;
use lash_sansio::sync::MutexExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Barrier;
use tokio::time::{Duration, timeout};

#[test]
fn standard_execution_section_uses_only_surviving_tool_examples() {
    for removed_tool in [
        "read_file",
        "\"edit\"",
        "\"write\"",
        "\"glob\"",
        "fetch_url",
        "search_web",
    ] {
        assert!(
            !STANDARD_EXECUTION_SECTION.contains(removed_tool),
            "standard prompt should not mention removed tool `{removed_tool}`"
        );
    }
    assert!(STANDARD_EXECUTION_SECTION.contains("declared JSON arguments"));
    assert!(STANDARD_EXECUTION_SECTION.contains("Check each batch result’s success flag"));
}

#[test]
fn protocol_message_ids_include_turn_identity() {
    let first = standard_message_id(&TurnId::from("turn-1"), 0, "assistant");
    let replay = standard_message_id(&TurnId::from("turn-1"), 0, "assistant");
    let next_turn = standard_message_id(&TurnId::from("turn-2"), 0, "assistant");

    assert_eq!(first, replay);
    assert_ne!(first, next_turn);
}

fn sequence_part(kind: usize, position: usize) -> (LlmOutputPart, PartKind, String) {
    match kind {
        0 => {
            let marker = format!("text-{position}");
            (
                LlmOutputPart::Text {
                    text: marker.clone(),
                    response_meta: None,
                },
                PartKind::Prose,
                marker,
            )
        }
        1 => {
            let marker = format!("reasoning-{position}");
            (
                LlmOutputPart::Reasoning {
                    text: marker.clone(),
                    replay: None,
                },
                PartKind::Reasoning,
                marker,
            )
        }
        2 => {
            let marker = format!("tool-{position}");
            (
                LlmOutputPart::ToolCall {
                    call_id: format!("call-{position}"),
                    tool_name: marker.clone(),
                    input_json: format!(r#"{{"position":{position}}}"#),
                    replay: None,
                },
                PartKind::ToolCall,
                marker,
            )
        }
        _ => unreachable!("base-three sequence kind"),
    }
}

#[test]
fn mixed_response_sequences_reassemble_in_arrival_order() {
    for len in 1_u32..=5 {
        for encoded in 0..3_usize.pow(len) {
            let mut cursor = encoded;
            let mut input = Vec::with_capacity(len as usize);
            let mut expected = Vec::with_capacity(len as usize);
            for position in 0..len as usize {
                let (part, kind, marker) = sequence_part(cursor % 3, position);
                cursor /= 3;
                input.push(part);
                expected.push((kind, marker));
            }

            let response = collect_standard_response(&LlmResponse {
                parts: input,
                ..LlmResponse::default()
            });
            let (actual, calls) = reassemble_standard_response("assistant", response.parts);

            assert_eq!(actual.len(), expected.len(), "sequence {encoded} len {len}");
            for (position, (actual, (expected_kind, marker))) in
                actual.iter().zip(expected.iter()).enumerate()
            {
                assert_eq!(
                    actual.kind(),
                    *expected_kind,
                    "kind at {position} in sequence {encoded} len {len}"
                );
                match actual.kind() {
                    PartKind::ToolCall => assert_eq!(
                        actual.tool_name(),
                        Some(marker.as_str()),
                        "tool marker at {position} in sequence {encoded} len {len}"
                    ),
                    _ => assert!(
                        actual.content().contains(marker),
                        "content marker at {position} in sequence {encoded} len {len}: {actual:?}"
                    ),
                }
            }
            assert_eq!(
                calls.len(),
                expected
                    .iter()
                    .filter(|(kind, _)| *kind == PartKind::ToolCall)
                    .count(),
                "tool dispatch count in sequence {encoded} len {len}"
            );
        }
    }
}

#[derive(Clone, Debug)]
struct WhitespaceInterleavedProvider;

#[async_trait::async_trait]
impl lash_core::facade_support::Provider for WhitespaceInterleavedProvider {
    fn kind(&self) -> &'static str {
        "stub"
    }

    fn route_identity(&self, model: &str) -> lash_core::ProviderRouteIdentity {
        lash_core::ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> lash_core::facade_support::ProviderOptions {
        lash_core::facade_support::ProviderOptions::default()
    }

    fn set_options(&mut self, _options: lash_core::facade_support::ProviderOptions) {}

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    async fn complete(
        &mut self,
        _request: lash_core::LlmRequest,
    ) -> Result<lash_core::LlmResponse, lash_core::facade_support::LlmTransportError> {
        Ok(lash_core::LlmResponse {
            parts: vec![
                lash_core::LlmOutputPart::Text {
                    text: "a".to_string(),
                    response_meta: None,
                },
                lash_core::LlmOutputPart::Text {
                    text: "   ".to_string(),
                    response_meta: None,
                },
                lash_core::LlmOutputPart::Reasoning {
                    text: "r".to_string(),
                    replay: None,
                },
                lash_core::LlmOutputPart::Text {
                    text: "b".to_string(),
                    response_meta: None,
                },
            ],
            ..lash_core::LlmResponse::default()
        })
    }

    fn clone_boxed(&self) -> Box<dyn lash_core::facade_support::Provider> {
        Box::new(self.clone())
    }
}

#[derive(Clone, Debug)]
struct BatchRuntimeProvider {
    calls: Arc<AtomicUsize>,
    saw_batch_result: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl lash_core::facade_support::Provider for BatchRuntimeProvider {
    fn kind(&self) -> &'static str {
        "stub"
    }

    fn route_identity(&self, model: &str) -> lash_core::ProviderRouteIdentity {
        lash_core::ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> lash_core::facade_support::ProviderOptions {
        lash_core::facade_support::ProviderOptions::default()
    }

    fn set_options(&mut self, _options: lash_core::facade_support::ProviderOptions) {}

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    async fn complete(
        &mut self,
        request: lash_core::LlmRequest,
    ) -> Result<lash_core::LlmResponse, lash_core::facade_support::LlmTransportError> {
        let call_index = self.calls.fetch_add(1, Ordering::SeqCst);
        if call_index == 0 {
            return Ok(lash_core::LlmResponse {
                parts: vec![lash_core::LlmOutputPart::ToolCall {
                    call_id: "batch-call".to_string(),
                    tool_name: "batch".to_string(),
                    input_json: serde_json::json!({
                        "tool_calls": [
                            {"tool": "alpha", "parameters": {}},
                            {"tool": "beta", "parameters": {"value": "fail"}},
                            {"tool": "internal_probe", "parameters": {}}
                        ]
                    })
                    .to_string(),
                    replay: None,
                }],
                response_metadata: Default::default(),
                ..lash_core::LlmResponse::default()
            });
        }

        let projected_messages = format!("{:?}", request.messages);
        if projected_messages.contains("alpha") && projected_messages.contains("beta failed") {
            self.saw_batch_result.store(true, Ordering::SeqCst);
        }
        Ok(lash_core::LlmResponse {
            parts: vec![lash_core::LlmOutputPart::Text {
                text: "done".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..lash_core::LlmResponse::default()
        })
    }

    fn clone_boxed(&self) -> Box<dyn lash_core::facade_support::Provider> {
        Box::new(self.clone())
    }
}

#[derive(Debug)]
struct BatchRuntimeTools {
    barrier: Arc<Barrier>,
    started: Arc<AtomicUsize>,
}

struct BatchRuntimeInternalTool {
    executed: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl lash_core::InternalProcessToolImplementation for BatchRuntimeInternalTool {
    async fn execute(
        &self,
        _call: lash_core::InternalProcessToolCall<'_>,
    ) -> lash_core::ToolOutcomeDone {
        self.executed.fetch_add(1, Ordering::SeqCst);
        lash_core::ToolOutcomeDone::ok(serde_json::json!("internal body ran"))
    }
}

pub(super) fn runtime_test_tool(name: &str) -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "",
        serde_json::json!({
            "type": "object",
            "properties": {
                "value": { "type": "string" }
            },
            "additionalProperties": true
        }),
        serde_json::json!({ "type": "string" }),
    )
}

#[async_trait::async_trait]
impl ToolProvider for BatchRuntimeTools {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![
            runtime_test_tool("alpha").manifest(),
            runtime_test_tool("beta").manifest(),
        ]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        match name {
            "alpha" | "beta" => Some(Arc::new(runtime_test_tool(name).contract())),
            _ => None,
        }
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.started.fetch_add(1, Ordering::SeqCst);
        if timeout(Duration::from_millis(100), self.barrier.wait())
            .await
            .is_err()
        {
            return ToolOutcome::err_fmt("batch child tools did not run concurrently").into();
        }
        if call.name() == "beta"
            && call.args.get("value").and_then(|value| value.as_str()) == Some("fail")
        {
            return ToolOutcome::err_fmt("beta failed").into();
        }
        ToolOutcome::ok(serde_json::json!(call.name())).into()
    }
}

type RecordedEffectFrame = (lash_core::RuntimeEffectKind, Option<String>);

/// Counts the effects that cross the backend's effect host, and answers the
/// turn's cancel-gate peeks unresolved. A layer over the backend host
/// (FIG-3580): group children minted under the host run under the same
/// layer, so the attempts a batch's children journal are counted too.
#[derive(Clone, Default)]
pub(super) struct CountingEffectController {
    pub(super) frames: Arc<std::sync::Mutex<Vec<RecordedEffectFrame>>>,
    pub(super) group_opens: Arc<AtomicUsize>,
}

impl CountingEffectController {
    fn count(&self, kind: lash_core::RuntimeEffectKind) -> usize {
        self.frames
            .lock_recover()
            .iter()
            .filter(|(candidate, _)| *candidate == kind)
            .count()
    }

    fn group_open_count(&self) -> usize {
        self.group_opens.load(Ordering::SeqCst)
    }

    fn tool_attempt_names(&self) -> Vec<String> {
        let mut names = self
            .frames
            .lock_recover()
            .iter()
            .filter_map(|(kind, name)| {
                (*kind == lash_core::RuntimeEffectKind::ToolAttempt)
                    .then(|| name.clone())
                    .flatten()
            })
            .collect::<Vec<_>>();
        names.sort();
        names
    }
}

#[async_trait::async_trait]
impl lash_core::testing::EffectLayer for CountingEffectController {
    async fn execute_effect(
        &self,
        inner: &dyn lash_core::RuntimeEffectController,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        let name = match &envelope.command {
            lash_core::RuntimeEffectCommand::ToolAttempt { call, .. } => {
                Some(call.tool_name.clone())
            }
            _ => None,
        };
        self.frames
            .lock_recover()
            .push((envelope.command.kind(), name));
        if matches!(
            &envelope.command,
            lash_core::RuntimeEffectCommand::PeekAwaitEvent { .. }
        ) {
            return Ok(lash_core::RuntimeEffectOutcome::PeekAwaitEvent { resolution: None });
        }
        inner.execute_effect(envelope, local_executor).await
    }

    async fn open_effect_group(
        &self,
        inner: &dyn lash_core::RuntimeEffectController,
        group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        self.group_opens.fetch_add(1, Ordering::SeqCst);
        inner.open_effect_group(group).await
    }
}

/// A SQLite memory backend (ADR 0102) and the runtime host config over it.
pub(super) async fn test_host() -> (
    lash_core::Backend,
    lash_core::facade_support::RuntimeHostConfig,
) {
    host_over(
        Arc::new(
            lash_sqlite_store::SqliteBackend::memory()
                .await
                .expect("open a SQLite memory backend"),
        )
        .into(),
    )
}

/// [`test_host`] with the backend's effect host under `layer`.
pub(super) async fn layered_test_host(
    layer: Arc<dyn lash_core::testing::EffectLayer>,
) -> (
    lash_core::Backend,
    lash_core::facade_support::RuntimeHostConfig,
) {
    // Layered before any host config is built over the backend: the first
    // config installs the host's one tool-child resolver, and a batch's
    // children must reach their controllers through the layer.
    host_over(
        lash_core::testing::runtime_helpers::LayeredBackend::over(
            Arc::new(
                lash_sqlite_store::SqliteBackend::memory()
                    .await
                    .expect("open a SQLite memory backend"),
            )
            .into(),
        )
        .map_effect_host(|host| Arc::new(lash_core::testing::LayeredEffectHost::new(host, layer)))
        .into_backend(),
    )
}

fn host_over(
    backend: lash_core::Backend,
) -> (
    lash_core::Backend,
    lash_core::facade_support::RuntimeHostConfig,
) {
    let host = lash_core::facade_support::RuntimeHostConfig::new(
        backend.clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    (backend, host)
}

/// `backend`'s effect host, scoped to `session_id`'s first turn.
pub(super) fn test_turn_scope(
    backend: &lash_core::Backend,
    session_id: &str,
) -> lash_core::ScopedEffectController<'static> {
    backend
        .effect_host()
        .scoped_static(lash_core::AdmittedScope::turn(session_id, "turn-1"))
        .expect("scoped controller")
        .expect("the backend host lends a static controller")
}

#[tokio::test]
async fn whitespace_only_text_does_not_split_terminal_history() {
    let provider_handle = lash_core::facade_support::ProviderHandle::new(
        lash_core::facade_support::ProviderComponents::new(Box::new(WhitespaceInterleavedProvider)),
    );
    let (backend, mut host) =
        layered_test_host(Arc::new(CountingEffectController::default())).await;
    host.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(provider_handle),
    );
    let policy = lash_core::SessionPolicy {
        provider_id: "stub".to_string(),
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("valid model"),
        // Bounded, not unbounded: these fixtures drive a live runtime loop
        // against a stub provider, so a driver that mistakes a tool-call-free
        // response for a tool-calling one spins here forever instead of
        // failing. The budget is well above the iterations the scenario needs.
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::bounded(8))
    };
    let scoped_controller = test_turn_scope(&backend, "whitespace-response-session");
    let mut runtime = Box::pin(
        lash_core::facade_support::LashRuntime::builder(
            host,
            lash_core::LeaseOwnerIdentity::opaque(
                "protocol-standard-test-worker",
                "protocol-standard-test-boot",
            ),
        )
        .with_session_id("whitespace-response-session")
        .with_policy(policy)
        .with_plugin_factories(vec![Arc::new(StandardProtocolPluginFactory::new())])
        .build(),
    )
    .await
    .expect("runtime");

    let turn = runtime
        .stream_turn(
            lash_core::TurnInput::text("respond with mixed parts"),
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                scoped_controller,
            ),
        )
        .await
        .expect("turn");

    let finish_text = match &turn.outcome {
        lash_core::facade_support::TurnOutcome::Finished(
            lash_core::facade_support::TurnFinish::AssistantMessage { text },
        ) => text,
        outcome => panic!("unexpected turn outcome: {outcome:?}"),
    };
    let read_view = turn
        .state
        .read_view()
        .expect("accepted turn frame scope resolves");
    let assistant_messages = read_view
        .messages()
        .iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .collect::<Vec<_>>();

    assert_eq!(
        assistant_messages.len(),
        1,
        "the terminal output must not materialize a duplicate assistant message"
    );
    let stored = assistant_messages[0];
    assert_eq!(
        stored
            .parts
            .iter()
            .map(|part| part.kind())
            .collect::<Vec<_>>(),
        [PartKind::Prose, PartKind::Reasoning, PartKind::Prose]
    );
    let rendered_text = stored
        .parts
        .iter()
        .filter(|part| {
            matches!(
                part.kind(),
                PartKind::Prose | PartKind::Text | PartKind::Attachment | PartKind::ToolResult
            )
        })
        .map(|part| part.content())
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(finish_text, &rendered_text);
}

#[tokio::test]
async fn standard_batch_is_runtime_owned_orchestration_without_an_enclosing_attempt() {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let saw_batch_result = Arc::new(AtomicBool::new(false));
    let provider = BatchRuntimeProvider {
        calls: Arc::clone(&provider_calls),
        saw_batch_result: Arc::clone(&saw_batch_result),
    };
    let provider_handle = lash_core::facade_support::ProviderHandle::new(
        lash_core::facade_support::ProviderComponents::new(Box::new(provider)),
    );
    // The counting layer sits over the backend's effect host, so the group
    // children the batch mints run under it too and their attempts land on
    // the same frame log the turn scope's counter reads.
    let controller = CountingEffectController::default();
    let (backend, mut host) = layered_test_host(Arc::new(controller.clone())).await;
    host.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(provider_handle),
    );
    let started = Arc::new(AtomicUsize::new(0));
    let internal_executed = Arc::new(AtomicUsize::new(0));
    let factories: Vec<Arc<dyn lash_core::facade_support::PluginFactory>> = vec![
        Arc::new(StandardProtocolPluginFactory::new()),
        Arc::new(lash_core::plugin::StaticPluginFactory::new(
            "standard-batch-test-tools",
            lash_core::facade_support::PluginSpec::new()
                .with_tool_provider(Arc::new(BatchRuntimeTools {
                    barrier: Arc::new(Barrier::new(2)),
                    started: Arc::clone(&started),
                }))
                .with_internal_tool(lash_core::InternalProcessToolDef::new(
                    runtime_test_tool("internal_probe"),
                    Arc::new(BatchRuntimeInternalTool {
                        executed: Arc::clone(&internal_executed),
                    }),
                )),
        )),
    ];
    let policy = lash_core::SessionPolicy {
        provider_id: "stub".to_string(),
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("valid model"),
        // Bounded, not unbounded: these fixtures drive a live runtime loop
        // against a stub provider, so a driver that mistakes a tool-call-free
        // response for a tool-calling one spins here forever instead of
        // failing. The budget is well above the iterations the scenario needs.
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::bounded(8))
    };
    let scoped_controller = test_turn_scope(&backend, "standard-batch-session");
    let mut runtime = Box::pin(
        lash_core::facade_support::LashRuntime::builder(
            host,
            lash_core::LeaseOwnerIdentity::opaque(
                "protocol-standard-test-worker",
                "protocol-standard-test-boot",
            ),
        )
        .with_session_id("standard-batch-session")
        .with_policy(policy)
        .with_plugin_factories(factories)
        .build(),
    )
    .await
    .expect("runtime");

    let turn = runtime
        .stream_turn(
            lash_core::TurnInput::text("run the batch"),
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                scoped_controller,
            ),
        )
        .await
        .expect("turn");

    assert!(matches!(
        turn.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
    assert_eq!(started.load(Ordering::SeqCst), 2);
    assert_eq!(
        internal_executed.load(Ordering::SeqCst),
        0,
        "a batch child must not cross normal admission into an Internal provider"
    );
    assert!(saw_batch_result.load(Ordering::SeqCst));
    assert_eq!(
        controller.group_open_count(),
        1,
        "each batch is a durable effect group now (FIG-3397)"
    );
    assert_eq!(
        controller.count(lash_core::RuntimeEffectKind::ToolAttempt),
        2,
        "only alpha and beta are attempts; the batch body itself has no ToolAttempt frame"
    );
    assert_eq!(
        controller.tool_attempt_names(),
        vec!["alpha".to_string(), "beta".to_string()],
        "the runtime-owned batch orchestration body is never enclosed by ToolAttempt"
    );
}

/// Provider stub whose first completion emits one tool call with invalid
/// argument JSON and whose second completion records the request messages
/// it was shown.
#[derive(Clone, Debug)]
struct MalformedArgsProvider {
    calls: Arc<AtomicUsize>,
    second_request: Arc<std::sync::Mutex<Option<String>>>,
}

#[async_trait::async_trait]
impl lash_core::facade_support::Provider for MalformedArgsProvider {
    fn kind(&self) -> &'static str {
        "stub"
    }

    fn route_identity(&self, model: &str) -> lash_core::ProviderRouteIdentity {
        lash_core::ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> lash_core::facade_support::ProviderOptions {
        lash_core::facade_support::ProviderOptions::default()
    }

    fn set_options(&mut self, _options: lash_core::facade_support::ProviderOptions) {}

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    async fn complete(
        &mut self,
        request: lash_core::LlmRequest,
    ) -> Result<lash_core::LlmResponse, lash_core::facade_support::LlmTransportError> {
        let call_index = self.calls.fetch_add(1, Ordering::SeqCst);
        if call_index == 0 {
            return Ok(lash_core::LlmResponse {
                parts: vec![lash_core::LlmOutputPart::ToolCall {
                    call_id: "malformed-call".to_string(),
                    tool_name: "status".to_string(),
                    input_json: r#"{"path": "a.txt", "content": "he said "hi"}"#.to_string(),
                    replay: None,
                }],
                ..lash_core::LlmResponse::default()
            });
        }
        *self.second_request.lock_recover() = Some(format!("{:?}", request.messages));
        Ok(lash_core::LlmResponse {
            parts: vec![lash_core::LlmOutputPart::Text {
                text: "done".to_string(),
                response_meta: None,
            }],
            ..lash_core::LlmResponse::default()
        })
    }

    fn clone_boxed(&self) -> Box<dyn lash_core::facade_support::Provider> {
        Box::new(self.clone())
    }
}

struct CountingToolProvider {
    executed: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolProvider for CountingToolProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![runtime_test_tool("status").manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        (name == "status").then(|| Arc::new(runtime_test_tool("status").contract()))
    }

    async fn execute(&self, _call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.executed.fetch_add(1, Ordering::SeqCst);
        ToolOutcome::ok(serde_json::json!("ran")).into()
    }
}

#[tokio::test]
async fn malformed_tool_arguments_are_refused_not_dispatched() {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let second_request = Arc::new(std::sync::Mutex::new(None));
    let provider_handle = lash_core::facade_support::ProviderHandle::new(
        lash_core::facade_support::ProviderComponents::new(Box::new(MalformedArgsProvider {
            calls: Arc::clone(&provider_calls),
            second_request: Arc::clone(&second_request),
        })),
    );
    let (backend, mut host) =
        layered_test_host(Arc::new(CountingEffectController::default())).await;
    host.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(provider_handle),
    );
    let executed = Arc::new(AtomicUsize::new(0));
    let factories: Vec<Arc<dyn lash_core::facade_support::PluginFactory>> = vec![
        Arc::new(StandardProtocolPluginFactory::new()),
        Arc::new(lash_core::plugin::StaticPluginFactory::new(
            "standard-malformed-args-test-tools",
            lash_core::facade_support::PluginSpec::new().with_tool_provider(Arc::new(
                CountingToolProvider {
                    executed: Arc::clone(&executed),
                },
            )),
        )),
    ];
    let policy = lash_core::SessionPolicy {
        provider_id: "stub".to_string(),
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("valid model"),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::bounded(8))
    };
    let scoped_controller = test_turn_scope(&backend, "malformed-args-session");
    let mut runtime = Box::pin(
        lash_core::facade_support::LashRuntime::builder(
            host,
            lash_core::LeaseOwnerIdentity::opaque(
                "protocol-standard-test-worker",
                "protocol-standard-test-boot",
            ),
        )
        .with_session_id("malformed-args-session")
        .with_policy(policy)
        .with_plugin_factories(factories)
        .build(),
    )
    .await
    .expect("runtime");

    let turn = runtime
        .stream_turn(
            lash_core::TurnInput::text("check status"),
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                scoped_controller,
            ),
        )
        .await
        .expect("turn");

    assert!(matches!(
        turn.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    assert_eq!(
        executed.load(Ordering::SeqCst),
        0,
        "a call whose arguments never parsed must never reach the tool body"
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);

    let projected = second_request
        .lock_recover()
        .clone()
        .expect("the refusal loops back to the model");
    for expected in [
        "invalid_tool_call_json",
        "not valid JSON",
        "line 1 column",
        "not executed",
    ] {
        assert!(
            projected.contains(expected),
            "the model-visible refusal must state `{expected}`: {projected}"
        );
    }

    // The assistant history keeps the model's raw argument text verbatim.
    let read_view = turn
        .state
        .read_view()
        .expect("accepted turn frame scope resolves");
    let tool_call_contents = read_view
        .messages()
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter(|part| part.kind() == PartKind::ToolCall)
        .map(|part| part.content().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        tool_call_contents,
        vec![r#"{"path": "a.txt", "content": "he said "hi"}"#.to_string()],
        "history must keep the model's raw argument text unchanged"
    );
}
