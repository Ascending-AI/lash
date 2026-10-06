use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core::plugin::{CodeExecutorPlugin, ProtocolSessionContext};
use lash_core::{SessionError, SessionHistoryRecord};
use lash_rlm_types::{RlmGlobalsPatchPluginBody, RlmProtocolEvent};

use crate::dialect::{DialectSession, SessionDialect};
use crate::projection::{RlmProjectedBindings, decode_rlm_protocol_event};

pub(crate) struct RlmRuntimeState {
    dialect: Arc<SessionDialect>,
    session_projected_bindings: tokio::sync::Mutex<RlmProjectedBindings>,
    execution: tokio::sync::Mutex<DialectSession>,
}

impl RlmRuntimeState {
    pub(crate) fn new(dialect: Arc<SessionDialect>) -> Result<Self, SessionError> {
        Ok(Self {
            execution: tokio::sync::Mutex::new(dialect.create_session()),
            dialect,
            session_projected_bindings: tokio::sync::Mutex::new(RlmProjectedBindings::new()),
        })
    }

    #[cfg(test)]
    pub(crate) fn new_for_tests() -> Result<Self, SessionError> {
        Self::new_for_tests_with_resolver(None)
    }

    /// A test session whose deferred-tool resolver can park a cell mid-flight.
    ///
    /// The resolver is awaited inside `execute_code_inner`, which is the only
    /// suspension point a unit test can reach without a live host: it lets a
    /// test hold a cell open and observe what a second caller — or a caller
    /// arriving after the first was cancelled — actually sees.
    #[cfg(test)]
    fn new_for_tests_with_resolver(
        deferred_tool_resolver: Option<lash_lashlang_runtime::SharedDeferredToolResolver>,
    ) -> Result<Self, SessionError> {
        let services = crate::dialect::RlmDialectServices {
            workers: lash_vm_client::service::Service::default(),
            artifact_store: crate::testing::sqlite_memory_artifact_store_blocking(),
            deferred_tool_resolver,
            deferred_trigger_resolver: None,

            execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
            code_renderer: Default::default(),
            channel: crate::plugin::RlmChannel::Cell,
        };
        Self::new(Arc::new(crate::dialect::SessionDialect::new(
            std::sync::Arc::new(crate::dialect::TypescriptDialect),
            lash_lashlang_runtime::LashlangSurface::default(),
            services,
        )))
    }

    /// The system prompt over the session's current bindings.
    pub(crate) async fn system_prompt(
        &self,
        behaviour: &crate::system_prompt::RlmSystemPromptBehaviour<'_>,
        prompt: &lash_rlm_types::RlmPrompt,
        tool_catalog: &lash_core::ToolCatalog,
        subagent: Option<&lash_core::SubagentSessionContext>,
        scope: crate::system_prompt::RlmSystemPromptScope,
    ) -> Arc<str> {
        let bindings = self.session_projected_bindings.lock().await;
        Arc::from(crate::system_prompt::render_system_prompt(
            &self.dialect,
            behaviour,
            crate::system_prompt::RlmSystemPromptInput {
                prompt,
                tool_catalog,
                bindings: &bindings,
                subagent,
            },
            scope,
        ))
    }

    /// The declaration of the session's read-only variables, as its system
    /// prompt renders it.
    #[cfg(test)]
    pub(crate) async fn read_only_variables_prompt(&self) -> Option<String> {
        let bindings = self.session_projected_bindings.lock().await;
        self.dialect.read_only_variables_prompt(&bindings)
    }

    /// Render the current bound-variables view on demand.
    ///
    /// The runtime calls this only where the result becomes a recorded input
    /// — the turn-machine build and each journaled execution-environment sync
    /// — so the projector never reads this state directly and a redrive
    /// replays the recorded render (FIG-3538).
    pub(crate) async fn bound_variables_prompt(
        &self,
        recorded: Option<&lash_core::RecordedRender>,
    ) -> Result<Arc<str>, SessionError> {
        let renderer = self.dialect.renderer();
        let recorded = lash_core::RecordedRender::require_available(recorded, renderer.0.id())
            .map_err(|code| SessionError::Protocol(code.to_string()))?;
        let params: crate::render::ResolvedRlmRender =
            serde_json::from_value(recorded.params.clone())
                .map_err(|error| SessionError::Protocol(error.to_string()))?;
        let exclude = self.protected_projected_binding_names().await;
        Ok(self
            .execution
            .lock()
            .await
            .prepare_bound_variables_prompt(&exclude, params.preview)?
            .render())
    }

    async fn protected_projected_binding_names(&self) -> BTreeSet<String> {
        self.session_projected_bindings
            .lock()
            .await
            .names()
            .collect()
    }

    /// Rebuild execution state and projected bindings from the restore view.
    ///
    /// Every restore replaces what this state holds (FIG-2521): the execution
    /// starts from a fresh dialect session, adopts the view's snapshot when the
    /// view carries one, and replays the view's seed and globals events; the
    /// projected bindings are rebuilt from those events alone. Restoring the
    /// frame this state already holds is therefore idempotent — a seed the
    /// view replays is re-bound rather than rejected — and nothing a failed
    /// operation left in the live execution survives: an append rolled back on
    /// commit failure or a follow-on turn whose commit was refused may have
    /// assigned globals or bound a projected name that never persisted, and
    /// the view is the only authority. The runtime builds that view from the
    /// committed state — the durable head, or the pre-append capture for an
    /// append rollback — so a view without a snapshot restores exactly what a
    /// cold reopen would build: the frame's replayed events on a fresh
    /// session.
    pub(crate) async fn restore_runtime_session_state(
        &self,
        state: lash_core::plugin::ProtocolSessionRestoreView,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<(), SessionError> {
        let mut execution_guard = self.execution.lock().await;
        let execution = &mut *execution_guard;
        let snapshot = state.execution_state.map_err(|error| SessionError::Store {
            context: "failed to hydrate RLM execution-state components".to_string(),
            source: error,
        })?;
        *execution = self.dialect.create_session();
        *self.session_projected_bindings.lock().await = RlmProjectedBindings::new();
        let protected_names = self.protected_projected_binding_names().await;
        if let Some(snapshot) = snapshot {
            execution
                .restore_execution_state(&snapshot, fleet_format)
                .await?;
            execution.prune_protected_globals(&protected_names).await?;
        }
        for event in &state.active_events {
            if let SessionHistoryRecord::Protocol(event) = event
                && let Some(event) = decode_rlm_protocol_event(event).map_err(history_corruption)?
            {
                self.apply_seed_or_globals_event(execution, event, &protected_names)
                    .await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn append_session_nodes(
        &self,
        nodes: &[lash_core::SessionAppendNode],
    ) -> Result<(), SessionError> {
        let mut execution_guard = self.execution.lock().await;
        let execution = &mut *execution_guard;
        let protected_names = self.protected_projected_binding_names().await;
        execution.prune_protected_globals(&protected_names).await?;
        for node in nodes {
            if let lash_core::SessionAppendNode::ProtocolEvent { event, .. } = node
                && let Some(event) = decode_rlm_protocol_event(event).map_err(history_corruption)?
            {
                self.apply_seed_or_globals_event(execution, event, &protected_names)
                    .await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn execute_code(
        &self,
        ctx: lash_core::RuntimeExecutionContext<'_>,
        request: lash_core::ExecRequest,
    ) -> Result<lash_core::ExecResponse, SessionError> {
        let session_projected_bindings = self.session_projected_bindings.lock().await.clone();
        // The guard is held across the whole cell: a second caller waits for
        // the cell to finish instead of being told the state is busy, and a
        // cell cancelled mid-flight leaves the state where it was.
        let mut guard = self.execution.lock().await;
        guard
            .execute(ctx, request, session_projected_bindings)
            .await
    }

    pub(crate) fn execution_state_dirty(&self) -> bool {
        // A contended `try_lock` means a cell is running, and a running cell
        // is dirty by construction.
        self.execution
            .try_lock()
            .map(|execution| execution.execution_state_dirty())
            .unwrap_or(true)
    }

    pub(crate) async fn snapshot_execution_state(
        &self,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<lash_core::plugin::ExecutionStateCapture, SessionError> {
        self.execution
            .lock()
            .await
            .snapshot_execution_state(fleet_format)
            .await
    }

    pub(crate) async fn probe_execution_state_capture(
        &self,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<(), SessionError> {
        self.execution
            .lock()
            .await
            .probe_execution_state_capture(fleet_format)
            .await
    }

    pub(crate) async fn hydrated_execution_state(
        &self,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<Option<lash_core::plugin::HydratedExecutionState>, SessionError> {
        self.execution
            .lock()
            .await
            .hydrated_execution_state(fleet_format)
            .await
            .map(Some)
    }

    pub(crate) async fn acknowledge_execution_state_capture(&self) {
        let _ = self
            .execution
            .lock()
            .await
            .acknowledge_execution_state_capture();
    }

    pub(crate) async fn abort_execution_state_capture(&self) {
        let _ = self.execution.lock().await.abort_execution_state_capture();
    }

    pub(crate) async fn settle_code_execution(
        &self,
        outcome: lash_core::plugin::CodeExecutionOutcome,
    ) -> Result<(), SessionError> {
        self.execution.lock().await.settle_code_execution(outcome)
    }

    pub(crate) async fn restore_execution_state(
        &self,
        state: &lash_core::plugin::HydratedExecutionState,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<(), SessionError> {
        self.execution
            .lock()
            .await
            .restore_execution_state(state, fleet_format)
            .await
    }

    async fn apply_seed_or_globals_event(
        &self,
        execution: &mut DialectSession,
        event: RlmProtocolEvent,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError> {
        match event {
            RlmProtocolEvent::RlmGlobalsPatch(patch) => {
                execution.patch_globals(&patch, protected_names).await?;
            }
            RlmProtocolEvent::RlmSeed(seed) => {
                let mut protected_names = protected_names.clone();
                if !seed.projected.is_empty() {
                    self.install_initial_projected_seed(seed.projected)?;
                    protected_names = self.protected_projected_binding_names().await;
                }
                if !seed.globals.is_empty() {
                    execution
                        .patch_globals(
                            &RlmGlobalsPatchPluginBody {
                                set_default: seed.globals,
                            },
                            &protected_names,
                        )
                        .await?;
                }
            }
            RlmProtocolEvent::RlmAssistantContent(_)
            | RlmProtocolEvent::RlmTrajectoryEntry(_)
            | RlmProtocolEvent::RlmDiagnostic(_) => {}
        }
        Ok(())
    }

    fn install_initial_projected_seed(
        &self,
        snapshot: lash_rlm_types::RlmProjectedSeedSnapshot,
    ) -> Result<(), SessionError> {
        let bindings = match RlmProjectedBindings::from_snapshot(&snapshot) {
            Ok(bindings) => bindings,
            Err(err) => {
                return Err(SessionError::Protocol(format!(
                    "rlm projected seed snapshot rejected: {err}"
                )));
            }
        };
        reject_reserved_projected_binding_names(&bindings)?;
        let mut guard = match self.session_projected_bindings.try_lock() {
            Ok(guard) => guard,
            Err(_) => return Err(SessionError::Protocol(
                "rlm projected seed snapshot could not be installed because session bindings were contended".to_string(),
            )),
        };
        let merged = guard
            .clone()
            .merge(bindings)
            .map_err(|err| SessionError::Protocol(err.to_string()))?;
        *guard = merged;
        Ok(())
    }
}

pub(crate) struct RlmCodeExecutor {
    state: Arc<RlmRuntimeState>,
}

impl RlmCodeExecutor {
    pub(crate) fn new(state: Arc<RlmRuntimeState>) -> Self {
        Self { state }
    }
}

#[async_trait::async_trait]
impl CodeExecutorPlugin for RlmCodeExecutor {
    async fn execute_code(
        &self,
        ctx: lash_core::RuntimeExecutionContext<'_>,
        request: lash_core::ExecRequest,
    ) -> Result<lash_core::ExecResponse, SessionError> {
        Box::pin(self.state.execute_code(ctx, request)).await
    }

    fn execution_state_dirty(&self) -> bool {
        self.state.execution_state_dirty()
    }

    async fn frame_switch_carries(
        &self,
        _ctx: ProtocolSessionContext<'_>,
        _successor: &lash_core::FrameNodeId,
        initial_nodes: &[lash_core::SessionAppendNode],
    ) -> Result<Vec<lash_core::ArtifactName>, SessionError> {
        frame_switch_carries(initial_nodes)
    }

    fn executable_generation(&self) -> Option<lash_core::ExecutableGeneration> {
        Some(lash_lashlang_runtime::lashlang_cell_generation())
    }

    async fn snapshot_execution_state(
        &self,
        ctx: ProtocolSessionContext<'_>,
    ) -> Result<lash_core::plugin::ExecutionStateCapture, SessionError> {
        self.state
            .snapshot_execution_state(ctx.fleet_format())
            .await
    }

    async fn probe_execution_state_capture(
        &self,
        ctx: ProtocolSessionContext<'_>,
    ) -> Result<(), SessionError> {
        self.state
            .probe_execution_state_capture(ctx.fleet_format())
            .await
    }

    async fn hydrated_execution_state(
        &self,
        ctx: ProtocolSessionContext<'_>,
    ) -> Result<Option<lash_core::plugin::HydratedExecutionState>, SessionError> {
        self.state
            .hydrated_execution_state(ctx.fleet_format())
            .await
    }

    async fn restore_execution_state(
        &self,
        ctx: ProtocolSessionContext<'_>,
        state: &lash_core::plugin::HydratedExecutionState,
    ) -> Result<(), SessionError> {
        self.state
            .restore_execution_state(state, ctx.fleet_format())
            .await
    }

    async fn acknowledge_execution_state_capture(&self) {
        self.state.acknowledge_execution_state_capture().await;
    }

    async fn abort_execution_state_capture(&self) {
        self.state.abort_execution_state_capture().await;
    }

    async fn settle_code_execution(
        &self,
        outcome: lash_core::plugin::CodeExecutionOutcome,
    ) -> Result<(), SessionError> {
        self.state.settle_code_execution(outcome).await
    }
}

/// The module artifacts a frame switch carries (ADR 0113 §3.1): every module
/// a value in the switch's seed and globals events references. Only these
/// survive into the successor frame; the ended frame's other modules are
/// severed once the switching turn settles.
fn frame_switch_carries(
    nodes: &[lash_core::SessionAppendNode],
) -> Result<Vec<lash_core::ArtifactName>, SessionError> {
    let mut definitions = BTreeSet::new();
    for node in nodes {
        let lash_core::SessionAppendNode::ProtocolEvent { event, .. } = node else {
            continue;
        };
        let values = match decode_rlm_protocol_event(event).map_err(history_corruption)? {
            Some(RlmProtocolEvent::RlmSeed(seed)) => serde_json::to_value(&seed),
            Some(RlmProtocolEvent::RlmGlobalsPatch(patch)) => serde_json::to_value(&patch),
            _ => continue,
        };
        // Both bodies are JSON maps, so encoding them cannot fail; a body
        // that did would carry nothing.
        if let Ok(values) = values {
            definitions.extend(lashlang::referenced_definition_ids(
                &crate::projection::json_to_flow_value(values.clone()),
            ));
        }
    }
    Ok(definitions
        .into_iter()
        .map(|id| lash_core::ArtifactName {
            store: lash_core::ArtifactStoreId::ProcessDefinition,
            artifact_ref: id.to_string(),
        })
        .collect())
}

pub(crate) fn reject_reserved_projected_binding_names(
    bindings: &RlmProjectedBindings,
) -> Result<(), SessionError> {
    if bindings.names().any(|name| name == "history") {
        return Err(SessionError::Protocol(
            "`history` is reserved as an RLM built-in binding".to_string(),
        ));
    }
    Ok(())
}

fn history_corruption(error: lash_core::StoredDataCorruption) -> SessionError {
    SessionError::Plugin(lash_core::PluginError::StoredDataCorrupt {
        record_kind: error.record_kind,
        message: error.message,
    })
}

#[cfg(test)]
#[allow(
    clippy::large_futures,
    reason = "the tests poll the one 16KB execute-code future from plain sync harnesses; boxing each call site would be noise without changing what is measured"
)]
mod tests {
    use super::*;

    /// A restore view for `frame` whose active history carries one RLM seed
    /// event per label, each binding `projected_<label>`.
    fn seed_restore_view(
        frame: &str,
        labels: &[&str],
    ) -> lash_core::plugin::ProtocolSessionRestoreView {
        restore_view(
            frame,
            None,
            labels
                .iter()
                .flat_map(|label| projected_seed_nodes(label))
                .collect(),
        )
    }

    /// A restore view for `frame` carrying `snapshot` and `nodes` as its
    /// active history.
    fn restore_view(
        frame: &str,
        snapshot: Option<lash_core::plugin::HydratedExecutionState>,
        nodes: Vec<lash_core::SessionAppendNode>,
    ) -> lash_core::plugin::ProtocolSessionRestoreView {
        lash_core::plugin::ProtocolSessionRestoreView {
            current_frame_node_id: Some(
                lash_core::FrameNodeId::new(frame).expect("test frame identity is non-empty"),
            ),
            execution_state: Ok(snapshot),
            active_events: nodes
                .into_iter()
                .map(|node| match node {
                    lash_core::SessionAppendNode::ProtocolEvent { event, .. } => {
                        SessionHistoryRecord::Protocol(event)
                    }
                    other => panic!("seed nodes are protocol events: {other:?}"),
                })
                .collect(),
        }
    }

    fn projected_seed_nodes(label: &str) -> Vec<lash_core::SessionAppendNode> {
        let mut seed = crate::projection::RlmSeed::default();
        seed.projected.push(
            format!("projected_{label}"),
            lash_rlm_types::RlmProjectedSeedEntry::Materialized(serde_json::json!({
                "value": label
            })),
        );
        crate::projection::rlm_seed_initial_nodes(seed, lash_core::FleetFormat::current())
    }

    fn projected_binding_names(declaration: &Option<String>) -> Vec<String> {
        let rendered = declaration.clone().unwrap_or_default();
        ["projected_seed", "projected_discarded"]
            .into_iter()
            .filter(|name| rendered.contains(name))
            .map(str::to_string)
            .collect()
    }

    /// FIG-2521: restoring the frame the state already holds is idempotent.
    ///
    /// The runtime restores the protocol session on the current frame after a
    /// follow-on failure, a reopen-seed receipt replay and an append rollback.
    /// Each replays the frame's seed events into a state that already bound
    /// them; the projected seed must be re-bound from the view, never rejected
    /// as a duplicate.
    #[test]
    fn same_frame_restore_rebinds_an_already_bound_projected_seed() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = RlmRuntimeState::new_for_tests().expect("state");
                state
                    .restore_runtime_session_state(
                        seed_restore_view("frame-1", &["seed"]),
                        lash_core::FleetFormat::current(),
                    )
                    .await
                    .expect("first restore binds the seed");
                let first = state.read_only_variables_prompt().await;
                assert_eq!(projected_binding_names(&first), ["projected_seed"]);

                state
                    .restore_runtime_session_state(
                        seed_restore_view("frame-1", &["seed"]),
                        lash_core::FleetFormat::current(),
                    )
                    .await
                    .expect("a same-frame restore re-binds the seed it already holds");
                let second = state.read_only_variables_prompt().await;
                assert_eq!(
                    format!("{second:?}"),
                    format!("{first:?}"),
                    "a same-frame restore rebuilds exactly the view's bindings"
                );
                assert_eq!(
                    projected_binding_names(&second),
                    ["projected_seed"],
                    "the seed is bound once, never twice"
                );
            });
    }

    /// FIG-2521: a same-frame restore replaces the live bindings with the
    /// view's, so a binding appended but never persisted (an append rolled
    /// back on commit failure) does not survive into the next prompt.
    #[test]
    fn same_frame_restore_drops_bindings_absent_from_the_restore_view() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = RlmRuntimeState::new_for_tests().expect("state");
                state
                    .restore_runtime_session_state(
                        seed_restore_view("frame-1", &["seed"]),
                        lash_core::FleetFormat::current(),
                    )
                    .await
                    .expect("restore binds the durable seed");
                let durable = state.read_only_variables_prompt().await;

                state
                    .append_session_nodes(&projected_seed_nodes("discarded"))
                    .await
                    .expect("the append binds the pending seed");
                let pending = state.read_only_variables_prompt().await;
                assert_eq!(
                    projected_binding_names(&pending),
                    ["projected_seed", "projected_discarded"]
                );

                state
                    .restore_runtime_session_state(
                        seed_restore_view("frame-1", &["seed"]),
                        lash_core::FleetFormat::current(),
                    )
                    .await
                    .expect("rolling back to the durable view on the same frame succeeds");
                let rolled_back = state.read_only_variables_prompt().await;
                assert_eq!(
                    format!("{rolled_back:?}"),
                    format!("{durable:?}"),
                    "the rolled-back append's binding must not survive the restore"
                );
            });
    }

    /// A process definition value naming a module built from `source`.
    fn definition_json(source: &str) -> (String, serde_json::Value) {
        let module_ref = lashlang::ModuleRef::new(&lashlang::ContentHash::new(source));
        let identity = lashlang::ProcessDefinitionIdentity::new(
            module_ref.clone(),
            lashlang::HostRequirementsRef::new(&lashlang::ContentHash::new("host")),
            lashlang::ProcessRef::new(lashlang::ContentHash::new("component"), 0),
            "run",
        );
        (
            identity.draft().expect("descriptor").id().to_string(),
            serde_json::to_value(
                identity
                    .definition(lash_core::ProcessSignature::Unknown)
                    .expect("definition"),
            )
            .expect("definition JSON"),
        )
    }

    #[test]
    fn a_frame_switch_carries_exactly_the_definition_ids_its_seed_references() {
        let (carried, carried_value) = definition_json("carried");
        let (projected, projected_value) = definition_json("projected");
        let mut seed = crate::projection::RlmSeed::from_seed_value(&serde_json::json!({
            "plain": 1,
            "nested": { "list": [carried_value.clone(), carried_value] },
        }))
        .expect("seed value");
        seed.projected.push(
            "projected_definition".to_string(),
            lash_rlm_types::RlmProjectedSeedEntry::Materialized(projected_value),
        );
        let mut nodes =
            crate::projection::rlm_seed_initial_nodes(seed, lash_core::FleetFormat::current());
        // A globals patch in the initial nodes is replayed into the new
        // frame too, so what it names is carried.
        let (patched, patched_value) = definition_json("patched");
        nodes.push(lash_core::SessionAppendNode::protocol_event(
            crate::projection::rlm_protocol_event(
                RlmProtocolEvent::RlmGlobalsPatch(RlmGlobalsPatchPluginBody {
                    set_default: serde_json::Map::from_iter([(
                        "patched".to_string(),
                        patched_value,
                    )]),
                }),
                lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
                    crate::RLM_PROTOCOL_EVENT_VERSION
                )),
            ),
        ));

        let mut expected = [carried, projected, patched]
            .into_iter()
            .map(|artifact_ref| lash_core::ArtifactName {
                store: lash_core::ArtifactStoreId::ProcessDefinition,
                artifact_ref,
            })
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(
            frame_switch_carries(&nodes).expect("valid history fixture"),
            expected
        );
        assert!(
            frame_switch_carries(&[])
                .expect("valid history fixture")
                .is_empty()
        );
    }
}
