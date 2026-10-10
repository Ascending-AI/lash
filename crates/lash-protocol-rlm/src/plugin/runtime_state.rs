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
        deferred_tool_resolver: Option<crate::SharedDeferredToolResolver>,
    ) -> Result<Self, SessionError> {
        let services = crate::dialect::RlmDialectServices {
            kernel: crate::executor::KernelCarry::default(),
            presentation: crate::RlmPresentationConfig::standard(),
            workers: lash_vm_client::service::Service::default(),
            deferred_tool_resolver,

            execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
            code_renderer: Default::default(),
            channel: crate::plugin::RlmChannel::Cell,
        };
        Self::new(Arc::new(crate::dialect::SessionDialect::new(
            crate::dialect::CellDialect::typescript(),
            services,
        )))
    }

    /// The declaration of the session's read-only variables, as its system
    /// prompt renders it.
    #[cfg(test)]
    pub(crate) async fn read_only_variables_prompt(&self) -> Option<String> {
        let bindings = self.session_projected_bindings.lock().await;
        self.dialect.read_only_variables_prompt(&bindings)
    }

    /// The facts the session's prompt sections render (ADR 0133): the
    /// current bound-variables view and the read-only variables.
    ///
    /// The runtime asks only where it composes a call's prompt. An admitted
    /// call records its prompt, so a resend never reads this state again.
    pub(crate) async fn prompt_facts(
        &self,
        recorded: Option<&lash_core::RecordedRender>,
        history: Option<&lash_core::SessionReadView>,
        images: bool,
    ) -> Result<crate::prompt_sections::RlmPromptFacts, SessionError> {
        let renderer = self.dialect.renderer();
        let recorded = lash_core::RecordedRender::require_available(recorded, renderer.0.id())
            .map_err(|code| SessionError::Protocol(code.to_string()))?;
        let params: crate::render::ResolvedRlmRender =
            serde_json::from_value(recorded.params.clone())
                .map_err(|error| SessionError::Protocol(error.to_string()))?;
        let read_only_variables = {
            let bindings = self.session_projected_bindings.lock().await;
            self.dialect.read_only_variables_prompt(&bindings)
        };
        let exclude = self.protected_projected_binding_names().await;
        let bound_variables = self
            .execution
            .lock()
            .await
            .prepare_bound_variables_prompt(&exclude, params.preview)
            .await?
            .render();
        Ok(crate::prompt_sections::RlmPromptFacts {
            history_binding: Arc::from(
                crate::prompt_sections::history_binding(
                    &self.dialect,
                    &history
                        .map(|view| view.chronological_projection())
                        .unwrap_or_default(),
                    images,
                )
                .map_err(history_corruption)?,
            ),
            bound_variables,
            read_only_variables,
        })
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
                if !seed.functions.is_empty() {
                    execution
                        .seed_functions(&seed.functions, &protected_names)
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

    /// The session's recorded presentation bound: at most
    /// `max_tool_call_records` records, each output's scalars cut at
    /// `max_inline_scalar_bytes`.
    fn bound_tool_call_records(
        &self,
        records: Vec<lash_core::ToolCallRecord>,
    ) -> (
        Vec<lash_core::ToolCallRecord>,
        Option<lash_core::OmittedToolCalls>,
    ) {
        crate::tool_records::bounded_exec_tool_call_records(
            &records,
            &self.state.dialect.presentation(),
        )
    }

    fn snapshot_tool_calls(
        &self,
        snapshot: &str,
    ) -> Result<Vec<lash_core::ToolCallRecord>, SessionError> {
        crate::executor::snapshot_tool_calls(snapshot).map_err(SessionError::Protocol)
    }

    async fn check_cell_snapshot(
        &self,
        snapshot: &str,
    ) -> Result<Result<(), String>, lash_core::RuntimeError> {
        Ok(crate::executor::check_cell_snapshot(snapshot))
    }

    async fn carried_cell_snapshot(
        &self,
        snapshot: &str,
    ) -> Result<
        Result<Option<lash_core::plugin::CarriedCellSnapshot>, String>,
        lash_core::RuntimeError,
    > {
        Ok(self
            .state
            .dialect
            .kernel()
            .cell_snapshot(snapshot)
            .map_err(|refusal| refusal.to_string()))
    }

    /// A session's cells publish no module artifacts: a frame switch has
    /// none to carry.
    async fn frame_switch_carries(
        &self,
        _ctx: ProtocolSessionContext<'_>,
        _successor: &lash_core::FrameNodeId,
        _initial_nodes: &[lash_core::SessionAppendNode],
    ) -> Result<Vec<lash_core::ArtifactName>, SessionError> {
        Ok(Vec::new())
    }

    fn executable_generation(&self) -> Option<lash_core::ExecutableGeneration> {
        Some(crate::executor::cell_generation())
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
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};

    #[test]
    fn projected_names_and_execution_captures_preserve_session_state() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = Arc::new(RlmRuntimeState::new_for_tests().expect("state"));
                let session = crate::plugin::protocol_session::RlmProtocolSession::new(
                    crate::plugin::RlmProtocolPluginConfig::builder()
                        .channel(crate::plugin::RlmChannel::Cell)
                        .instruction_limit(crate::plugin::InstructionBound::unbounded())
                        .memory_limit(crate::plugin::MemoryBound::mebibytes(64))
                        .build(),
                    state.clone(),
                );
                let session_id = lash_core::SessionId::from("mutation-session");
                let ctx = || {
                    lash_core::plugin::ProtocolSessionContext::new(
                        &session_id,
                        lash_core::FleetFormat::current(),
                    )
                };
                lash_core::plugin::ProtocolSessionPlugin::append_session_nodes(
                    &session,
                    ctx(),
                    &projected_seed_nodes("kept"),
                )
                .await
                .expect("seed");
                assert_eq!(
                    state.protected_projected_binding_names().await,
                    BTreeSet::from(["projected_kept".to_string()])
                );
                {
                    let _running = state.execution.lock().await;
                    assert!(state.execution_state_dirty(), "a running cell is dirty");
                }
                state
                    .snapshot_execution_state(lash_core::FleetFormat::current())
                    .await
                    .expect("capture idle state");
                state.acknowledge_execution_state_capture().await;
                assert!(
                    !state.execution_state_dirty(),
                    "an acknowledged idle session is clean"
                );
                let response = execute_cell(
                    &state,
                    cell("let persisted = 4815; await control.finish(persisted);"),
                )
                .await
                .expect("cell");
                assert_eq!(
                    response.finish_value().cloned(),
                    Some(serde_json::json!(4815))
                );
                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionOutcome::Accepted)
                    .await
                    .expect("settle");
                assert!(
                    state.execution_state_dirty(),
                    "an accepted cell has uncaptured changes"
                );
                let capture = state
                    .snapshot_execution_state(lash_core::FleetFormat::current())
                    .await
                    .expect("capture");
                assert!(
                    capture.root().is_some_and(|root| !root.is_empty()),
                    "the capture carries the execution state"
                );
                lash_core::plugin::ProtocolSessionPlugin::restore_session(
                    &session,
                    ctx(),
                    seed_restore_view("restored", &["restored"]),
                )
                .await
                .expect("restore through the protocol hook");
                assert_eq!(
                    state.protected_projected_binding_names().await,
                    BTreeSet::from(["projected_restored".to_string()])
                );
            });
    }

    /// Runs `request` on `state` as one cell, under a durable host of its
    /// own.
    async fn execute_cell(
        state: &RlmRuntimeState,
        request: lash_core::ExecRequest,
    ) -> Result<lash_core::ExecResponse, SessionError> {
        let handler = crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
        let (provider, catalog) = crate::testing::with_finish(Arc::new(crate::testing::NoTools));
        Box::pin(state.execute_code(
            lash_core::testing::code_execution_context_with_tool_provider_and_catalog(
                handler.ports(),
                provider,
                catalog,
            ),
            request,
        ))
        .await
    }

    /// The scope [`admitted_context`] claims.
    fn admitted_scope() -> lash_core::AdmittedScope {
        lash_core::AdmittedScope::turn(
            lash_core::SessionId::from("runtime-state-session"),
            lash_core::TurnId::from("runtime-state-turn"),
        )
    }

    /// A cell that references an unresolved module call-path, so the deferred
    /// resolver is consulted before anything is linked or run.
    const PARKING_CELL: &str = "await web.fetch({});";

    /// A deferred-tool resolver that parks a cell inside `resolve` until it is
    /// released.
    ///
    /// This is the only suspension point a unit test can plant in the middle of
    /// a cell without a live host, and it is what makes the two properties
    /// under test observable at all: what a *second* caller sees while a cell
    /// is running, and what the session looks like after a cell is cancelled
    /// while running.
    #[derive(Default)]
    struct ParkingResolver {
        entered: AtomicUsize,
        released: AtomicBool,
    }

    impl ParkingResolver {
        fn entered(&self) -> usize {
            self.entered.load(Ordering::SeqCst)
        }

        fn release(&self) {
            self.released.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl crate::DeferredToolResolver for ParkingResolver {
        async fn resolve(
            &self,
            _cx: &crate::DeferredResolveContext<'_>,
            paths: &[&str],
        ) -> std::collections::BTreeMap<String, crate::Resolution> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            // A self-waking park: every poll re-reads the flag, so a release
            // is never missed whichever waker happens to execute the future.
            std::future::poll_fn(|cx| {
                if self.released.load(Ordering::SeqCst) {
                    Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
            paths
                .iter()
                .map(|path| ((*path).to_string(), crate::Resolution::NotAvailable))
                .collect()
        }
    }

    fn parked_session() -> (Arc<ParkingResolver>, RlmRuntimeState) {
        let resolver = Arc::new(ParkingResolver::default());
        let state = RlmRuntimeState::new_for_tests_with_resolver(Some(
            Arc::clone(&resolver) as crate::SharedDeferredToolResolver
        ))
        .expect("runtime state");
        (resolver, state)
    }

    fn cell(code: &str) -> lash_core::ExecRequest {
        lash_core::ExecRequest {
            code: code.to_string(),
        }
    }

    /// A cell context under `cell_id`'s invocation, under the claimed
    /// context `handler` lends for [`admitted_scope`].
    fn admitted_context(
        handler: &crate::testing::DurableHost,
        cell_id: &str,
    ) -> lash_core::RuntimeExecutionContext<'static> {
        let replay_key = format!("exec-code:{cell_id}");
        let (provider, catalog) = crate::testing::with_finish(Arc::new(crate::testing::NoTools));
        lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
            handler.ports(),
            provider,
            catalog,
            lash_core::testing::exec_code_invocation(
                "runtime-state-session",
                "runtime-state-turn",
                0,
                0,
                replay_key.clone(),
                replay_key,
            ),
        )
    }

    /// Shift `future` until it is parked inside the resolver, i.e. suspended in
    /// the middle of a cell with the execution state in hand.
    ///
    /// Each poll runs on the task's own waker and the loop yields between
    /// polls, so reaching the resolver takes real turns of the runtime rather
    /// than a fixed number of polls.
    async fn poll_until_parked<F: Future>(future: &mut Pin<Box<F>>, resolver: &ParkingResolver) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while resolver.entered() == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the cell never reached the deferred resolver"
            );
            let polled = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(future.as_mut().poll(cx).is_pending())
            })
            .await;
            assert!(polled, "a cell parked in the resolver cannot complete");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }

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

    fn baton_seed_nodes(label: &str) -> Vec<lash_core::SessionAppendNode> {
        let mut seed =
            crate::projection::RlmSeed::from_seed_value(&serde_json::json!({ "baton": label }))
                .expect("seed value");
        seed.projected.push(
            format!("projected_{label}"),
            lash_rlm_types::RlmProjectedSeedEntry::Materialized(serde_json::json!({
                "value": label
            })),
        );
        crate::projection::rlm_seed_initial_nodes(seed, lash_core::FleetFormat::current())
    }

    /// The value `finish baton` yields on the state's live execution.
    async fn live_baton(state: &RlmRuntimeState) -> serde_json::Value {
        let response = execute_cell(state, cell("await control.finish(baton);"))
            .await
            .expect("the baton cell runs");
        state
            .settle_code_execution(lash_core::plugin::CodeExecutionOutcome::Accepted)
            .await
            .expect("settle the baton cell");
        assert_eq!(response.error(), None);
        response
            .finish_value()
            .cloned()
            .expect("the baton cell finishes")
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

    /// The regression test for the defect FIG-1729 fixes.
    ///
    /// The old code moved the execution state out of its holder for the
    /// duration of the cell, so a future dropped mid-cell dropped the state
    /// with it and left `None` behind for good: every later call on that
    /// session failed with the busy protocol error, permanently. The state is
    /// now only borrowed, so a cancelled cell leaves it exactly where it was
    /// and the next cell runs.
    #[test]
    fn a_cancelled_cell_leaves_the_session_usable() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let (resolver, state) = parked_session();

                // Execute a cell until it is suspended mid-flight, then drop it:
                // a cancellation with the execution state in the cell's hands.
                let cancelled_handler = crate::testing::DurableHost::open(admitted_scope()).await;
                {
                    let mut cancelled = Box::pin(state.execute_code(
                        admitted_context(&cancelled_handler, "cancelled"),
                        cell(PARKING_CELL),
                    ));
                    poll_until_parked(&mut cancelled, &resolver).await;
                }

                // The state was borrowed, never moved out, so the session is
                // still whole and the next cell runs normally.
                let handler = crate::testing::DurableHost::open(admitted_scope()).await;
                let next = state
                    .execute_code(
                        admitted_context(&handler, "survivor"),
                        cell("let survivor = 1;\nawait control.finish(survivor);"),
                    )
                    .await
                    .expect("the session survives a cell cancelled mid-flight");
                assert_eq!(next.error(), None);
                assert_eq!(next.finish_value().cloned(), Some(serde_json::json!(1)));
            });
    }

    #[test]
    fn a_second_concurrent_cell_waits_and_then_runs() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let (resolver, state) = parked_session();
                let mut cx = Context::from_waker(Waker::noop());

                // One cell is running: parked mid-flight, holding the state.
                let running_handler = crate::testing::DurableHost::open(admitted_scope()).await;
                let mut running = Box::pin(state.execute_code(
                    admitted_context(&running_handler, "running"),
                    cell(PARKING_CELL),
                ));
                poll_until_parked(&mut running, &resolver).await;

                // A second cell arrives while the first is still running. It
                // makes no progress whatsoever: it is queued behind the running
                // cell rather than answered — with a result or with an error.
                let waiting_handler = crate::testing::DurableHost::open(admitted_scope()).await;
                {
                    let mut waiting = Box::pin(state.execute_code(
                        admitted_context(&waiting_handler, "waiting"),
                        cell("let second_cell = 2;"),
                    ));
                    for _ in 0..16 {
                        assert!(
                            waiting.as_mut().poll(&mut cx).is_pending(),
                            "a cell arriving mid-cell must wait for the running cell"
                        );
                    }
                    assert_eq!(
                        resolver.entered(),
                        1,
                        "the waiting cell never began executing beside the running one"
                    );
                }

                // The running cell finishes and hands the state back, live.
                resolver.release();
                let first = running.await.expect("the parked cell completes");
                assert!(
                    first.error().is_some(),
                    "`web.fetch` resolves to nothing, so the parked cell ends in a link error"
                );
                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionOutcome::Accepted)
                    .await
                    .expect("settle the first returned cell");

                // The waiting cell, redriven, now runs — on that same state.
                let handler = crate::testing::DurableHost::open(admitted_scope()).await;
                let second = state
                    .execute_code(
                        admitted_context(&handler, "waiting"),
                        cell("let second_cell = 2;"),
                    )
                    .await
                    .expect("the cell that waited now runs");
                assert_eq!(second.error(), None);
                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionOutcome::Accepted)
                    .await
                    .expect("settle the second returned cell");

                let handler = crate::testing::DurableHost::open(admitted_scope()).await;
                let total = state
                    .execute_code(
                        admitted_context(&handler, "total"),
                        cell("await control.finish(second_cell);"),
                    )
                    .await
                    .expect("execute code");
                assert_eq!(total.error(), None);
                assert_eq!(total.finish_value().cloned(), Some(serde_json::json!(2)));
            });
    }

    #[test]
    fn a_returned_cell_must_be_settled_before_another_cell_can_start() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = RlmRuntimeState::new_for_tests().expect("runtime state");
                execute_cell(&state, cell("let first = 1;"))
                    .await
                    .expect("first cell");

                let overlapping = execute_cell(&state, cell("let second = 2;"))
                    .await
                    .expect_err("an unsettled response fences the next cell");
                assert!(matches!(
                    overlapping,
                    SessionError::Protocol(message)
                        if message == "the previous code execution response has not been settled"
                ));

                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionOutcome::Accepted)
                    .await
                    .expect("settle first response");
                execute_cell(&state, cell("let second = 2;"))
                    .await
                    .expect("settlement releases the next cell");
            });
    }

    #[test]
    fn executing_code_updates_the_bound_variables_render() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = RlmRuntimeState::new_for_tests().expect("runtime state");
                let render = crate::testing::recorded_test_render();
                let prompt = state
                    .prompt_facts(Some(&render), None, true)
                    .await
                    .expect("prompt facts")
                    .bound_variables;
                assert!(!prompt.contains("scratch_note"));

                execute_cell(
                    &state,
                    lash_core::ExecRequest {
                        code: "let scratch_note = \"after execution\";".to_string(),
                    },
                )
                .await
                .expect("execute code");

                let prompt = state
                    .prompt_facts(Some(&render), None, true)
                    .await
                    .expect("prompt facts")
                    .bound_variables;
                assert!(prompt.contains(r#"- `scratch_note` = "after execution""#));
            });
    }

    /// FIG-5772: the bindings inventory lists a saved function with its
    /// signature and the cell whose end froze what it reads.
    #[test]
    fn a_saved_function_is_listed_with_its_signature_and_the_cell_that_froze_it() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = RlmRuntimeState::new_for_tests().expect("runtime state");
                let render = crate::testing::recorded_test_render();
                execute_cell(
                    &state,
                    cell("const rate = 2;\nfunction scale(value: number): number { return value * rate; }"),
                )
                .await
                .expect("execute code");

                let prompt = state
                    .prompt_facts(Some(&render), None, true)
                    .await
                    .expect("prompt facts")
                    .bound_variables;
                assert!(
                    prompt.contains("`scale`")
                        && prompt.contains(
                            "function (value: number) => number; captures frozen at cell 1"
                        ),
                    "{prompt}"
                );
            });
    }

    #[test]
    fn cancelled_settlement_restores_the_bound_variables_render() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = RlmRuntimeState::new_for_tests().expect("runtime state");
                let render = crate::testing::recorded_test_render();

                execute_cell(&state, cell("let survives = 7;"))
                    .await
                    .expect("execute accepted cell");
                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionOutcome::Accepted)
                    .await
                    .expect("accept first cell");
                execute_cell(&state, cell("let cancelled_tail = 1;"))
                    .await
                    .expect("execute cell before late cancellation");
                let rendered = state
                    .prompt_facts(Some(&render), None, true)
                    .await
                    .expect("prompt facts")
                    .bound_variables;
                assert!(rendered.contains("cancelled_tail"));

                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionOutcome::Cancelled)
                    .await
                    .expect("cancel second cell");
                let rendered = state
                    .prompt_facts(Some(&render), None, true)
                    .await
                    .expect("prompt facts")
                    .bound_variables;
                assert!(rendered.contains("survives"));
                assert!(!rendered.contains("cancelled_tail"));
            });
    }

    /// A restore view for `frame` whose active history carries one RLM seed

    #[test]
    fn same_frame_restore_without_a_snapshot_discards_uncommitted_execution() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = RlmRuntimeState::new_for_tests().expect("state");
                state
                    .restore_runtime_session_state(
                        restore_view("frame-1", None, baton_seed_nodes("committed")),
                        lash_core::FleetFormat::current(),
                    )
                    .await
                    .expect("restore seeds the committed baton");
                assert_eq!(
                    Box::pin(live_baton(&state)).await,
                    serde_json::json!("committed")
                );

                let mutated = execute_cell(
                    &state,
                    cell("let baton = \"uncommitted\";\nawait control.finish(baton);"),
                )
                .await
                .expect("the mutating cell runs");
                assert_eq!(
                    mutated.finish_value().cloned(),
                    Some(serde_json::json!("uncommitted"))
                );
                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionOutcome::Accepted)
                    .await
                    .expect("settle the mutating cell");

                state
                    .restore_runtime_session_state(
                        restore_view("frame-1", None, baton_seed_nodes("committed")),
                        lash_core::FleetFormat::current(),
                    )
                    .await
                    .expect("a same-frame restore without a snapshot succeeds");
                assert_eq!(
                    Box::pin(live_baton(&state)).await,
                    serde_json::json!("committed"),
                    "the restore must rebuild the execution from the view, not keep the \
                     uncommitted assignment"
                );
            });
    }

    /// FIG-2521: a same-frame restore whose view carries an execution snapshot
    /// replaces the live execution with that snapshot. This is the view the
    /// runtime builds for an append rollback (the pre-append capture) and for a
    /// reload whose durable head carries an execution root.
    #[test]
    fn same_frame_restore_with_a_snapshot_replaces_the_live_execution() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = RlmRuntimeState::new_for_tests().expect("state");
                state
                    .restore_runtime_session_state(
                        restore_view("frame-1", None, baton_seed_nodes("committed")),
                        lash_core::FleetFormat::current(),
                    )
                    .await
                    .expect("restore seeds the committed baton");
                let committed = state
                    .hydrated_execution_state(lash_core::FleetFormat::current())
                    .await
                    .expect("capture")
                    .expect("the RLM executor always holds a snapshotable state");

                let mutated = execute_cell(
                    &state,
                    cell("let baton = \"uncommitted\";\nawait control.finish(baton);"),
                )
                .await
                .expect("the mutating cell runs");
                assert_eq!(
                    mutated.finish_value().cloned(),
                    Some(serde_json::json!("uncommitted"))
                );
                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionOutcome::Accepted)
                    .await
                    .expect("settle the mutating cell");

                state
                    .restore_runtime_session_state(
                        restore_view(
                            "frame-1",
                            Some(committed.clone()),
                            baton_seed_nodes("committed"),
                        ),
                        lash_core::FleetFormat::current(),
                    )
                    .await
                    .expect("a same-frame restore with a snapshot succeeds");
                // Compared before any further cell runs: executing a cell pins
                // this execution's child attempt bound into the root, which is
                // a write the restore itself must not make.
                assert_eq!(
                    state
                        .hydrated_execution_state(lash_core::FleetFormat::current())
                        .await
                        .expect("capture")
                        .expect("state"),
                    committed,
                    "the restored execution is exactly the view's snapshot"
                );
                assert_eq!(
                    Box::pin(live_baton(&state)).await,
                    serde_json::json!("committed")
                );
            });
    }
}
