use super::*;
use lash_core::EffectHost as _;
use lash_sansio::sync::MutexExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Barrier;
use tokio::time::{Duration, timeout};

#[test]
fn standard_protocol_factory_id_is_stable_plugin_contract() {
    let factory = StandardProtocolPluginFactory::new();

    assert_eq!(factory.id(), STANDARD_PROTOCOL_PLUGIN_ID);
    assert_eq!(factory.id(), "standard_protocol");
}

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

#[derive(Clone, Default)]
pub(super) struct CountingEffectController {
    pub(super) frames: Arc<std::sync::Mutex<Vec<RecordedEffectFrame>>>,
    pub(super) group_opens: Arc<AtomicUsize>,
    /// The native controller group operations forward to: a tool batch is
    /// a durable effect group now, so a test double that refuses groups
    /// leaves every batch unrouted. Shared with the runtime's effect host
    /// so group opens and group-child admission see the same group map.
    pub(super) native: Arc<lash_core::facade_support::NativeRuntimeEffectController>,
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

#[derive(Default)]
struct DurableMemoryAttachmentStore {
    inner: lash_core::facade_support::InMemoryAttachmentStore,
}

#[async_trait::async_trait]
impl lash_core::AttachmentStore for DurableMemoryAttachmentStore {
    fn persistence(&self) -> lash_core::AttachmentStorePersistence {
        lash_core::AttachmentStorePersistence::Durable
    }

    async fn put(
        &self,
        bytes: Vec<u8>,
        meta: lash_core::AttachmentCreateMeta,
    ) -> Result<lash_core::AttachmentRef, lash_core::AttachmentStoreError> {
        self.inner.put(bytes, meta).await
    }

    async fn get(
        &self,
        id: &lash_core::AttachmentId,
    ) -> Result<lash_core::StoredAttachment, lash_core::AttachmentStoreError> {
        self.inner.get(id).await
    }

    async fn delete(
        &self,
        id: &lash_core::AttachmentId,
    ) -> Result<(), lash_core::AttachmentStoreError> {
        self.inner.delete(id).await
    }

    async fn list(&self) -> Result<Vec<lash_core::StoredBlobRef>, lash_core::AttachmentStoreError> {
        self.inner.list().await
    }

    async fn head(
        &self,
        id: &lash_core::AttachmentId,
    ) -> Result<Option<lash_core::StoredBlobRef>, lash_core::AttachmentStoreError> {
        self.inner.head(id).await
    }
}

#[derive(Default)]
struct DurableMemoryProcessEnvStore {
    inner: lash_core::facade_support::InMemoryProcessExecutionEnvStore,
}

#[async_trait::async_trait]
impl lash_core::ProcessExecutionEnvStore for DurableMemoryProcessEnvStore {
    async fn publish_process_execution_env(
        &self,
        owner: &lash_core::ArtifactOwner,
        env_ref: &lash_core::ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> Result<(), lash_core::PluginError> {
        self.inner
            .publish_process_execution_env(owner, env_ref, bytes)
            .await
    }

    async fn transfer_process_execution_env(
        &self,
        from: &lash_core::ArtifactOwner,
        to: &lash_core::ArtifactOwner,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> Result<(), lash_core::PluginError> {
        self.inner
            .transfer_process_execution_env(from, to, env_ref)
            .await
    }

    async fn release_process_execution_env(
        &self,
        owner: &lash_core::ArtifactOwner,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> Result<(), lash_core::PluginError> {
        self.inner
            .release_process_execution_env(owner, env_ref)
            .await
    }

    async fn retire_process_execution_env_owner(
        &self,
        owner: &lash_core::ArtifactOwner,
    ) -> Result<(), lash_core::PluginError> {
        self.inner.retire_process_execution_env_owner(owner).await
    }

    async fn get_process_execution_env(
        &self,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> Result<Option<Vec<u8>>, lash_core::PluginError> {
        self.inner.get_process_execution_env(env_ref).await
    }
}

impl lash_core::AwaitEventResolver for CountingEffectController {}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for CountingEffectController {
    async fn execute_effect(
        &self,
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
        local_executor.execute(envelope).await
    }

    async fn open_effect_group(
        &self,
        group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        self.group_opens.fetch_add(1, Ordering::SeqCst);
        self.native.open_effect_group(group).await
    }

    fn register_group_executors(
        &self,
        executors: Arc<dyn lash_core::GroupExecutors>,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.native.register_group_executors(executors)
    }

    fn native_effect_groups_substrate(&self) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        self.native.native_effect_groups_substrate()
    }

    async fn await_next_settlement(
        &self,
        handle: &mut lash_core::EffectGroupHandle,
        cancel: lash_core::CancellationToken,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        self.native.await_next_settlement(handle, cancel).await
    }

    async fn close_effect_group(
        &self,
        handle: lash_core::EffectGroupHandle,
        disposition: lash_core::LoserPolicy,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.native.close_effect_group(handle, disposition).await
    }

    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<
        Option<lash_core::runtime::effect::RankedGroupSettlement>,
        lash_core::RuntimeEffectControllerError,
    > {
        self.native.read_group_settlement(group_key, rank).await
    }

    fn group_child_scoped_controller(
        &self,
        admitted: lash_core::AdmittedScope,
        binding: lash_core::GroupChildBinding,
    ) -> Result<Option<lash_core::ScopedEffectController<'static>>, lash_core::RuntimeError> {
        let Some(inner) = self
            .native
            .group_child_scoped_controller(admitted.clone(), binding)?
        else {
            return Ok(None);
        };
        Ok(Some(lash_core::ScopedEffectController::shared(
            Arc::new(CountingBoundController {
                frames: Arc::clone(&self.frames),
                inner,
            }),
            admitted,
        )?))
    }

    async fn commit_group_child_final(
        &self,
        commit: lash_core::facade_support::effect_replay_driver::GroupChildFinalCommit,
    ) -> Result<
        lash_core::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome,
        lash_core::RuntimeEffectControllerError,
    > {
        self.native.commit_group_child_final(commit).await
    }

    async fn group_child_drain_blocked(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<bool, lash_core::RuntimeEffectControllerError> {
        self.native
            .group_child_drain_blocked(group_key, commit_seq)
            .await
    }
}

/// A group child's bound controller wrapped in the same frame counter the
/// turn scope uses. Under the group shape a batch's attempts journal through
/// the child's controller — minted by the host, not the turn's scoped
/// controller — so without this wrap `count(ToolAttempt)` would see nothing
/// the children ran (FIG-3397).
struct CountingBoundController {
    frames: Arc<std::sync::Mutex<Vec<RecordedEffectFrame>>>,
    inner: lash_core::ScopedEffectController<'static>,
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for CountingBoundController {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.inner.controller().await_event_authority_binding_id()
    }

    async fn acquire_queued_lane(
        &self,
        lane: Arc<dyn lash_core::QueuedLaneProbe>,
        cancel: lash_core::CancellationToken,
    ) -> Result<lash_core::QueuedLaneAcquisition, lash_core::RuntimeError> {
        self.inner
            .controller()
            .acquire_queued_lane(lane, cancel)
            .await
    }

    async fn wait_out_crashed_lane_holder(
        &self,
        lane: Arc<dyn lash_core::QueuedLaneProbe>,
        cancel: lash_core::CancellationToken,
    ) -> Result<lash_core::QueuedLaneAcquisition, lash_core::RuntimeError> {
        self.inner
            .controller()
            .wait_out_crashed_lane_holder(lane, cancel)
            .await
    }

    async fn prepare_completion_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<lash_core::CompletionKeyPreparation, lash_core::RuntimeError> {
        self.inner
            .controller()
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }

    async fn await_event_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> Result<lash_core::AwaitEventKey, lash_core::RuntimeError> {
        self.inner.controller().await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveOutcome, lash_core::RuntimeError> {
        self.inner
            .controller()
            .resolve_await_event(key, resolution)
            .await
    }

    async fn peek_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> Result<Option<lash_core::Resolution>, lash_core::RuntimeError> {
        self.inner.controller().peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        cancel: lash_core::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<lash_core::Resolution, lash_core::RuntimeError> {
        self.inner
            .controller()
            .await_await_event(key, cancel, deadline)
            .await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &lash_core::SessionId,
    ) -> Result<(), lash_core::RuntimeError> {
        self.inner
            .controller()
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &lash_core::SessionId,
    ) -> Result<(), lash_core::RuntimeError> {
        self.inner
            .controller()
            .cancel_await_events_for_session(session_id)
            .await
    }

    async fn retire_await_events_for_scope(
        &self,
        scope: &lash_core::ExecutionScope,
    ) -> Result<(), lash_core::RuntimeError> {
        self.inner
            .controller()
            .retire_await_events_for_scope(scope)
            .await
    }

    async fn retire_await_events_for_scope_if_quiescent(
        &self,
        scope: &lash_core::ExecutionScope,
    ) -> Result<bool, lash_core::RuntimeError> {
        self.inner
            .controller()
            .retire_await_events_for_scope_if_quiescent(scope)
            .await
    }

    async fn reinstate_await_event_scope(
        &self,
        scope: &lash_core::ExecutionScope,
    ) -> Result<(), lash_core::RuntimeError> {
        self.inner
            .controller()
            .reinstate_await_event_scope(scope)
            .await
    }

    async fn await_event_scope_is_retired(
        &self,
        scope: &lash_core::ExecutionScope,
    ) -> Result<bool, lash_core::RuntimeError> {
        self.inner
            .controller()
            .await_event_scope_is_retired(scope)
            .await
    }
}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for CountingBoundController {
    fn effect_journaling(&self) -> lash_core::EffectJournaling {
        self.inner.controller().effect_journaling()
    }

    async fn execute_effect(
        &self,
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
        self.inner
            .controller()
            .execute_effect(envelope, local_executor)
            .await
    }

    async fn open_effect_group(
        &self,
        group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        self.inner.controller().open_effect_group(group).await
    }

    fn register_group_executors(
        &self,
        executors: Arc<dyn lash_core::GroupExecutors>,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.inner.controller().register_group_executors(executors)
    }

    fn native_effect_groups_substrate(&self) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        self.inner.controller().native_effect_groups_substrate()
    }

    async fn await_next_settlement(
        &self,
        handle: &mut lash_core::EffectGroupHandle,
        cancel: lash_core::CancellationToken,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        self.inner
            .controller()
            .await_next_settlement(handle, cancel)
            .await
    }

    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<
        Option<lash_core::runtime::effect::RankedGroupSettlement>,
        lash_core::RuntimeEffectControllerError,
    > {
        self.inner
            .controller()
            .read_group_settlement(group_key, rank)
            .await
    }

    async fn close_effect_group(
        &self,
        handle: lash_core::EffectGroupHandle,
        disposition: lash_core::LoserPolicy,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.inner
            .controller()
            .close_effect_group(handle, disposition)
            .await
    }

    fn group_child_scoped_controller(
        &self,
        admitted: lash_core::AdmittedScope,
        binding: lash_core::GroupChildBinding,
    ) -> Result<Option<lash_core::ScopedEffectController<'static>>, lash_core::RuntimeError> {
        self.inner
            .controller()
            .group_child_scoped_controller(admitted, binding)
    }

    async fn commit_group_child_final(
        &self,
        commit: lash_core::facade_support::effect_replay_driver::GroupChildFinalCommit,
    ) -> Result<
        lash_core::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome,
        lash_core::RuntimeEffectControllerError,
    > {
        self.inner
            .controller()
            .commit_group_child_final(commit)
            .await
    }

    async fn group_child_drain_blocked(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<bool, lash_core::RuntimeEffectControllerError> {
        self.inner
            .controller()
            .group_child_drain_blocked(group_key, commit_seq)
            .await
    }
}

/// An effect host that delegates to the native host but mints
/// counting-wrapped controllers for group children, so the test's frame log
/// sees the attempts children journal under their own bound controllers
/// (FIG-3397).
struct CountingEffectHost {
    inner: lash_core::facade_support::NativeEffectHost,
    frames: Arc<std::sync::Mutex<Vec<RecordedEffectFrame>>>,
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for CountingEffectHost {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.inner.await_event_authority_binding_id()
    }

    async fn prepare_completion_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<lash_core::CompletionKeyPreparation, lash_core::RuntimeError> {
        self.inner
            .await_event_resolver()
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }

    async fn await_event_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> Result<lash_core::AwaitEventKey, lash_core::RuntimeError> {
        self.inner
            .await_event_resolver()
            .await_event_key(scope, wait)
            .await
    }

    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveOutcome, lash_core::RuntimeError> {
        self.inner
            .await_event_resolver()
            .resolve_await_event(key, resolution)
            .await
    }

    async fn peek_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> Result<Option<lash_core::Resolution>, lash_core::RuntimeError> {
        self.inner
            .await_event_resolver()
            .peek_await_event(key)
            .await
    }

    async fn await_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        cancel: lash_core::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<lash_core::Resolution, lash_core::RuntimeError> {
        self.inner
            .await_event_resolver()
            .await_await_event(key, cancel, deadline)
            .await
    }
}

#[async_trait::async_trait]
impl lash_core::EffectHost for CountingEffectHost {
    fn turn_control_binding_id(&self) -> String {
        self.inner.turn_control_binding_id()
    }

    fn await_event_resolver(&self) -> &dyn lash_core::AwaitEventResolver {
        self
    }

    async fn list_outstanding_await_event_keys(
        &self,
        session_id: &lash_core::SessionId,
    ) -> Result<Vec<lash_core::AwaitEventKey>, lash_core::RuntimeError> {
        self.inner
            .list_outstanding_await_event_keys(session_id)
            .await
    }

    fn scoped<'run>(
        &'run self,
        scope: lash_core::AdmittedScope,
    ) -> Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        self.inner.scoped(scope)
    }

    fn scoped_static(
        &self,
        scope: lash_core::AdmittedScope,
    ) -> Result<Option<lash_core::ScopedEffectController<'static>>, lash_core::RuntimeError> {
        self.inner.scoped_static(scope)
    }

    fn scoped_for_group_child(
        &self,
        admitted: lash_core::AdmittedScope,
        binding: lash_core::GroupChildBinding,
    ) -> Result<Option<lash_core::ScopedEffectController<'static>>, lash_core::RuntimeError> {
        let Some(inner) = self
            .inner
            .scoped_for_group_child(admitted.clone(), binding)?
        else {
            return Ok(None);
        };
        Ok(Some(lash_core::ScopedEffectController::shared(
            Arc::new(CountingBoundController {
                frames: Arc::clone(&self.frames),
                inner,
            }),
            admitted,
        )?))
    }

    fn effect_group_closing(
        &self,
    ) -> Option<Arc<dyn lash_core::runtime::effect::StoreEffectGroupClosing>> {
        self.inner.effect_group_closing()
    }

    fn install_tool_child_host(
        &self,
        candidate: Arc<lash_core::facade_support::ToolChildHost>,
    ) -> Option<Arc<lash_core::facade_support::ToolChildHost>> {
        self.inner.install_tool_child_host(candidate)
    }

    async fn prepare_tool_intent(
        &self,
        sink: &dyn lash_core::ToolIntentOutcomeSink,
        identity: &lash_core::ToolIntentIdentity,
        intent: lash_core::ToolIntent,
    ) -> Result<lash_core::ToolIntentPreparation, lash_core::RuntimeError> {
        self.inner.prepare_tool_intent(sink, identity, intent).await
    }

    async fn record_tool_intent_outcome(
        &self,
        sink: &dyn lash_core::ToolIntentOutcomeSink,
        identity: &lash_core::ToolIntentIdentity,
        submitted: lash_core::ToolIntent,
        outcome: lash_core::ToolIntentExecutionOutcome,
    ) -> Result<(), lash_core::RuntimeError> {
        self.inner
            .record_tool_intent_outcome(sink, identity, submitted, outcome)
            .await
    }

    async fn retire_effect_journal(
        &self,
        retirement: lash_core::runtime::effect::EffectJournalRetirement,
    ) -> Result<usize, lash_core::RuntimeError> {
        self.inner.retire_effect_journal(retirement).await
    }

    async fn reinstate_effect_scope(
        &self,
        scope: &lash_core::ExecutionScope,
    ) -> Result<(), lash_core::RuntimeError> {
        self.inner.reinstate_effect_scope(scope).await
    }
}

#[tokio::test]
async fn whitespace_only_text_does_not_split_terminal_history() {
    let provider_handle = lash_core::facade_support::ProviderHandle::new(
        lash_core::facade_support::ProviderComponents::new(Box::new(WhitespaceInterleavedProvider)),
    );
    let mut host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
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
    let scoped_controller = lash_core::ScopedEffectController::shared(
        Arc::new(CountingEffectController::default()),
        lash_core::AdmittedScope::turn("whitespace-response-session", "turn-1"),
    )
    .expect("scoped controller");
    let mut runtime = Box::pin(
        lash_core::facade_support::LashRuntime::builder(
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
            lash_core::LeaseOwnerIdentity::opaque(
                "protocol-standard-test-worker",
                "protocol-standard-test-boot",
            ),
        )
        .with_session_id("whitespace-response-session")
        .with_policy(policy)
        .with_runtime_host(host)
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
    // The counting double and the host's effect host share one native
    // controller: group opens land there, and group-child admission under
    // the tool-child host (installed by RuntimeHostConfig::new) reads the
    // same controller's group map. The host is counting-wrapped so the
    // attempts a group child journals under its own bound controller land on
    // the same frame log the turn-scope counter reads.
    let native = Arc::new(lash_core::facade_support::NativeRuntimeEffectController::default());
    let frames: Arc<std::sync::Mutex<Vec<RecordedEffectFrame>>> = Default::default();
    let mut host = lash_core::facade_support::RuntimeHostConfig::new(
        Arc::new(CountingEffectHost {
            inner: lash_core::facade_support::NativeEffectHost::with_native_controller(Arc::clone(
                &native,
            )),
            frames: Arc::clone(&frames),
        }),
        Arc::new(lash_core::facade_support::InMemoryAttachmentStore::new()),
        Arc::new(lash_core::facade_support::InMemoryProcessExecutionEnvStore::new()),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(provider_handle),
    );
    host.durability.attachment_store = Arc::new(
        lash_core::facade_support::SessionAttachmentStore::ephemeral(Arc::new(
            DurableMemoryAttachmentStore::default(),
        )),
    );
    // Through the builder, not a field write: the installed tool-child host
    // resolves a child's recorded `execution_env` against the same store and
    // must hear about the swap.
    host = host.with_process_env_store(Arc::new(DurableMemoryProcessEnvStore::default()));
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
    let controller = CountingEffectController {
        frames,
        native,
        ..Default::default()
    };
    let scoped_controller = lash_core::ScopedEffectController::shared(
        Arc::new(controller.clone()),
        lash_core::AdmittedScope::turn("standard-batch-session", "turn-1"),
    )
    .expect("scoped controller");
    let mut runtime = Box::pin(
        lash_core::facade_support::LashRuntime::builder(
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
            lash_core::LeaseOwnerIdentity::opaque(
                "protocol-standard-test-worker",
                "protocol-standard-test-boot",
            ),
        )
        .with_session_id("standard-batch-session")
        .with_policy(policy)
        .with_runtime_host(host)
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
    let mut host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(provider_handle),
    );
    host.durability.attachment_store = Arc::new(
        lash_core::facade_support::SessionAttachmentStore::ephemeral(Arc::new(
            DurableMemoryAttachmentStore::default(),
        )),
    );
    host.durability.process_env_store = Arc::new(DurableMemoryProcessEnvStore::default());
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
    let scoped_controller = lash_core::ScopedEffectController::shared(
        Arc::new(CountingEffectController::default()),
        lash_core::AdmittedScope::turn("malformed-args-session", "turn-1"),
    )
    .expect("scoped controller");
    let mut runtime = Box::pin(
        lash_core::facade_support::LashRuntime::builder(
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
            lash_core::LeaseOwnerIdentity::opaque(
                "protocol-standard-test-worker",
                "protocol-standard-test-boot",
            ),
        )
        .with_session_id("malformed-args-session")
        .with_policy(policy)
        .with_runtime_host(host)
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
