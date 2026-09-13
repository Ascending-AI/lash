use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core::plugin::{CodeExecutorPlugin, ProtocolSessionContext};
use lash_core::{SessionError, SessionHistoryRecord};
use lash_rlm_types::{RlmGlobalsPatchPluginBody, RlmProtocolEvent};

use crate::dialect::{RlmDialect, RlmDialectRegistry, RlmDialectSession};
use crate::projection::{RlmProjectedBindings, RlmProjectionExtension, decode_rlm_protocol_event};
use crate::rlm_support::SharedBoundVariablesPrompt;

pub(crate) struct RlmRuntimeState {
    dialect_registry: RlmDialectRegistry,
    dialect: Arc<dyn RlmDialect>,
    session_projected_bindings: tokio::sync::Mutex<RlmProjectedBindings>,
    execution: tokio::sync::Mutex<Box<dyn RlmDialectSession>>,
    bound_variables_prompt: SharedBoundVariablesPrompt,
}

impl RlmRuntimeState {
    pub(crate) fn new(
        dialect_registry: RlmDialectRegistry,
        dialect: Arc<dyn RlmDialect>,
    ) -> Result<Self, SessionError> {
        let execution = dialect.create_session()?;
        let bound_variables_prompt = Arc::new(std::sync::RwLock::new(
            execution
                .prepare_bound_variables_prompt(&BTreeSet::new())?
                .render(),
        ));
        Ok(Self {
            execution: tokio::sync::Mutex::new(execution),
            dialect_registry,
            dialect,
            session_projected_bindings: tokio::sync::Mutex::new(RlmProjectedBindings::new()),
            bound_variables_prompt,
        })
    }

    #[cfg(test)]
    pub(crate) fn new_lashlang_for_tests() -> Result<Self, SessionError> {
        Self::new_for_tests("lashlang")
    }

    #[cfg(test)]
    fn new_for_tests(active_language: &str) -> Result<Self, SessionError> {
        Self::new_for_tests_with_resolver(active_language, None)
    }

    /// A test session whose deferred-tool resolver can park a cell mid-flight.
    ///
    /// The resolver is awaited inside `execute_code_inner`, which is the only
    /// suspension point a unit test can reach without a live host: it lets a
    /// test hold a cell open and observe what a second caller — or a caller
    /// arriving after the first was cancelled — actually sees.
    #[cfg(test)]
    fn new_for_tests_with_resolver(
        active_language: &str,
        deferred_tool_resolver: Option<lash_lashlang_runtime::SharedDeferredToolResolver>,
    ) -> Result<Self, SessionError> {
        let services = crate::dialect::LashlangDialectServices {
            projection_resolver: Arc::new(crate::projection::ProjectionRegistry::new()),
            artifact_store: lashlang::global_in_memory_lashlang_artifact_store(),
            deferred_tool_resolver,
            execution_trace_config: crate::executor::RlmLashlangExecutionTraceConfig::default(),
            execution_bounds: crate::plugin::ExecutionBounds::unbounded(),
            channel: crate::plugin::RlmChannel::Cell,
        };
        let dialect: Arc<dyn RlmDialect> = Arc::new(crate::dialect::LashlangDialect::new(
            lash_lashlang_runtime::LashlangSurface::default(),
            services.clone(),
        ));
        let typescript: Arc<dyn RlmDialect> = Arc::new(crate::dialect::TypescriptDialect::new(
            lash_lashlang_runtime::LashlangSurface::default(),
            services,
        ));
        let active = match active_language {
            "lashlang" => Arc::clone(&dialect),
            "typescript" => Arc::clone(&typescript),
            other => panic!("unknown test dialect `{other}`"),
        };
        Self::new(RlmDialectRegistry::new([dialect, typescript]), active)
    }

    pub(crate) async fn projected_binding_prompt_contributions(
        &self,
    ) -> Vec<lash_core::PromptContribution> {
        let bindings = self.session_projected_bindings.lock().await;
        RlmProjectionExtension::prompt_contributions_for(
            &bindings,
            self.dialect.prompt_vocabulary(),
        )
    }

    pub(crate) fn shared_bound_variables_prompt(&self) -> SharedBoundVariablesPrompt {
        Arc::clone(&self.bound_variables_prompt)
    }

    async fn refresh_bound_variables_prompt(&self) -> Result<(), SessionError> {
        let exclude = self.protected_projected_binding_names().await;
        let rendered = self
            .execution
            .lock()
            .await
            .prepare_bound_variables_prompt(&exclude)?
            .render();
        *self
            .bound_variables_prompt
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = rendered;
        Ok(())
    }

    async fn protected_projected_binding_names(&self) -> BTreeSet<String> {
        self.session_projected_bindings
            .lock()
            .await
            .names()
            .collect()
    }

    pub(crate) async fn apply_session_extension(
        &self,
        extension: lash_core::ProtocolSessionExtensionHandle,
    ) -> Result<(), SessionError> {
        let extension = extension
            .as_any()
            .downcast_ref::<RlmProjectionExtension>()
            .ok_or_else(|| {
                SessionError::Protocol(
                    "RLM protocol received an unsupported session extension".to_string(),
                )
            })?;
        reject_reserved_projected_binding_names(&extension.bindings)?;
        let mut guard = self.session_projected_bindings.lock().await;
        let merged = guard
            .clone()
            .merge(extension.bindings.clone())
            .map_err(|err| SessionError::Protocol(err.to_string()))?;
        *guard = merged;
        drop(guard);
        self.refresh_bound_variables_prompt().await?;
        Ok(())
    }

    pub(crate) async fn validate_turn_extension(
        &self,
        extension: &lash_core::ProtocolTurnExtensionHandle,
    ) -> Result<(), SessionError> {
        let extension = extension
            .as_any()
            .downcast_ref::<RlmProjectionExtension>()
            .ok_or_else(|| {
                SessionError::Protocol(
                    "RLM protocol received an unsupported turn extension".to_string(),
                )
            })?;
        reject_reserved_projected_binding_names(&extension.bindings)?;
        self.session_projected_bindings
            .lock()
            .await
            .clone()
            .merge(extension.bindings.clone())
            .map(|_| ())
            .map_err(|err| SessionError::Protocol(err.to_string()))
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
    ) -> Result<(), SessionError> {
        let mut execution_guard = self.execution.lock().await;
        let execution = &mut *execution_guard;
        let snapshot = state.execution_state.map_err(|error| SessionError::Store {
            context: "failed to hydrate RLM execution-state components".to_string(),
            source: error,
        })?;
        *execution = self.dialect.create_session()?;
        *self.session_projected_bindings.lock().await = RlmProjectedBindings::new();
        let protected_names = self.protected_projected_binding_names().await;
        if let Some(snapshot) = snapshot {
            execution.restore_execution_state(&snapshot)?;
            execution.prune_protected_globals(&protected_names)?;
        }
        for event in &state.active_events {
            if let SessionHistoryRecord::Protocol(event) = event
                && let Some(event) = decode_rlm_protocol_event(event)
            {
                self.apply_seed_or_globals_event(execution.as_mut(), event, &protected_names)
                    .await?;
            }
        }
        drop(execution_guard);
        self.refresh_bound_variables_prompt().await?;
        Ok(())
    }

    pub(crate) async fn append_session_nodes(
        &self,
        nodes: &[lash_core::SessionAppendNode],
    ) -> Result<(), SessionError> {
        let mut execution_guard = self.execution.lock().await;
        let execution = &mut *execution_guard;
        let protected_names = self.protected_projected_binding_names().await;
        execution.prune_protected_globals(&protected_names)?;
        for node in nodes {
            if let lash_core::SessionAppendNode::ProtocolEvent { event, .. } = node
                && let Some(event) = decode_rlm_protocol_event(event)
            {
                self.apply_seed_or_globals_event(execution.as_mut(), event, &protected_names)
                    .await?;
            }
        }
        drop(execution_guard);
        self.refresh_bound_variables_prompt().await?;
        Ok(())
    }

    pub(crate) async fn execute_code(
        &self,
        ctx: lash_core::RuntimeExecutionContext<'_>,
        request: lash_core::ExecRequest,
    ) -> Result<lash_core::ExecResponse, SessionError> {
        self.dialect_registry
            .resolve_active(&request.language, self.dialect.language_id())
            .map_err(|error| SessionError::Protocol(error.to_string()))?;
        let session_projected_bindings = self.session_projected_bindings.lock().await.clone();
        // The guard is held across the whole cell: a second caller waits for
        // the cell to finish instead of being told the state is busy, and a
        // cell cancelled mid-flight leaves the state where it was.
        let mut guard = self.execution.lock().await;
        let result = guard
            .as_mut()
            .execute(ctx, request, session_projected_bindings)
            .await;
        drop(guard);
        self.refresh_bound_variables_prompt().await?;
        result
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
    ) -> Result<lash_core::plugin::ExecutionStateSnapshot, SessionError> {
        self.execution.lock().await.snapshot_execution_state()
    }

    pub(crate) async fn probe_execution_state_capture(&self) -> Result<(), SessionError> {
        self.execution.lock().await.probe_execution_state_capture()
    }

    pub(crate) async fn hydrated_execution_state(
        &self,
    ) -> Result<Option<lash_core::plugin::HydratedExecutionState>, SessionError> {
        self.execution
            .lock()
            .await
            .hydrated_execution_state()
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
        disposition: lash_core::plugin::CodeExecutionDisposition,
    ) -> Result<(), SessionError> {
        let rolled_back = disposition != lash_core::plugin::CodeExecutionDisposition::Accepted;
        self.execution
            .lock()
            .await
            .settle_code_execution(disposition)?;
        if rolled_back {
            self.refresh_bound_variables_prompt().await?;
        }
        Ok(())
    }

    pub(crate) async fn restore_execution_state(
        &self,
        state: &lash_core::plugin::HydratedExecutionState,
    ) -> Result<(), SessionError> {
        let mut execution = self.execution.lock().await;
        execution.restore_execution_state(state)?;
        drop(execution);
        self.refresh_bound_variables_prompt().await?;
        Ok(())
    }

    async fn apply_seed_or_globals_event(
        &self,
        execution: &mut dyn RlmDialectSession,
        event: RlmProtocolEvent,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError> {
        match event {
            RlmProtocolEvent::RlmGlobalsPatch(patch) => {
                execution.patch_globals(&patch, protected_names)?;
            }
            RlmProtocolEvent::RlmSeed(seed) => {
                let mut protected_names = protected_names.clone();
                if !seed.projected.is_empty() {
                    self.install_initial_projected_seed(seed.projected)?;
                    protected_names = self.protected_projected_binding_names().await;
                }
                if !seed.globals.is_empty() {
                    execution.patch_globals(
                        &RlmGlobalsPatchPluginBody {
                            set_default: seed.globals,
                        },
                        &protected_names,
                    )?;
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
        self.state.execute_code(ctx, request).await
    }

    fn execution_state_dirty(&self) -> bool {
        self.state.execution_state_dirty()
    }

    async fn snapshot_execution_state(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<lash_core::plugin::ExecutionStateSnapshot, SessionError> {
        self.state.snapshot_execution_state().await
    }

    async fn probe_execution_state_capture(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<(), SessionError> {
        self.state.probe_execution_state_capture().await
    }

    async fn hydrated_execution_state(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<Option<lash_core::plugin::HydratedExecutionState>, SessionError> {
        self.state.hydrated_execution_state().await
    }

    async fn restore_execution_state(
        &self,
        _ctx: ProtocolSessionContext<'_>,
        state: &lash_core::plugin::HydratedExecutionState,
    ) -> Result<(), SessionError> {
        self.state.restore_execution_state(state).await
    }

    async fn acknowledge_execution_state_capture(&self) {
        self.state.acknowledge_execution_state_capture().await;
    }

    async fn abort_execution_state_capture(&self) {
        self.state.abort_execution_state_capture().await;
    }

    async fn settle_code_execution(
        &self,
        disposition: lash_core::plugin::CodeExecutionDisposition,
    ) -> Result<(), SessionError> {
        self.state.settle_code_execution(disposition).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};

    /// A cell that references an unresolved module call-path, so the deferred
    /// resolver is consulted before anything is linked or run.
    const PARKING_CELL: &str = "await web.fetch({})?";

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
    impl lash_lashlang_runtime::DeferredToolResolver for ParkingResolver {
        async fn resolve(
            &self,
            paths: &[&str],
        ) -> std::collections::BTreeMap<String, lash_lashlang_runtime::Resolution> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            // A self-waking park: every poll re-reads the flag, so a release
            // is never missed whichever waker happens to drive the future.
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
                .map(|path| {
                    (
                        (*path).to_string(),
                        lash_lashlang_runtime::Resolution::NotAvailable,
                    )
                })
                .collect()
        }
    }

    fn parked_session() -> (Arc<ParkingResolver>, RlmRuntimeState) {
        let resolver = Arc::new(ParkingResolver::default());
        let state = RlmRuntimeState::new_for_tests_with_resolver(
            "lashlang",
            Some(Arc::clone(&resolver) as lash_lashlang_runtime::SharedDeferredToolResolver),
        )
        .expect("runtime state");
        (resolver, state)
    }

    fn cell(code: &str) -> lash_core::ExecRequest {
        lash_core::ExecRequest {
            language: "lashlang".to_string(),
            code: code.to_string(),
        }
    }

    fn admitted_context(cell_id: &str) -> lash_core::RuntimeExecutionContext<'static> {
        let replay_key = format!("exec-code:{cell_id}");
        lash_core::testing::code_execution_context_with_invocation(
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

    /// Drive `future` until it is parked inside the resolver, i.e. suspended in
    /// the middle of a cell with the execution state in hand.
    fn poll_until_parked<F: Future>(
        future: &mut Pin<Box<F>>,
        cx: &mut Context<'_>,
        resolver: &ParkingResolver,
    ) {
        for _ in 0..64 {
            if resolver.entered() > 0 {
                return;
            }
            assert!(
                future.as_mut().poll(cx).is_pending(),
                "a cell parked in the resolver cannot complete"
            );
        }
        panic!("the cell never reached the deferred resolver");
    }

    /// The regression test for the defect FIG-1729 fixes.
    ///
    /// The old code moved the execution state out of its holder for the
    /// duration of the cell, so a future dropped mid-cell dropped the state
    /// with it and left `None` behind for good: every later call on that
    /// session failed with the busy protocol error, permanently. The state is
    /// now only borrowed, so a cancelled cell leaves it exactly where it was
    /// and the next cell runs.
    ///
    /// This test fails on the parent commit and passes here.
    #[test]
    fn a_cancelled_cell_leaves_the_session_usable() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let (resolver, state) = parked_session();
                let mut cx = Context::from_waker(Waker::noop());

                // Drive a cell until it is suspended mid-flight, then drop it:
                // a cancellation with the execution state in the cell's hands.
                {
                    let mut cancelled = Box::pin(
                        state.execute_code(admitted_context("cancelled"), cell(PARKING_CELL)),
                    );
                    poll_until_parked(&mut cancelled, &mut cx, &resolver);
                }

                // The state was borrowed, never moved out, so the session is
                // still whole and the next cell runs normally.
                let next = state
                    .execute_code(
                        admitted_context("survivor"),
                        cell("survivor = 1\nfinish survivor"),
                    )
                    .await
                    .expect("the session survives a cell cancelled mid-flight");
                assert_eq!(next.error, None);
                assert_eq!(next.terminal_finish, Some(serde_json::json!(1)));
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
                let mut running =
                    Box::pin(state.execute_code(admitted_context("running"), cell(PARKING_CELL)));
                poll_until_parked(&mut running, &mut cx, &resolver);

                // A second cell arrives while the first is still running. It
                // makes no progress whatsoever: it is queued behind the running
                // cell rather than answered — with a result or with an error.
                {
                    let mut waiting = Box::pin(
                        state.execute_code(admitted_context("waiting"), cell("second_cell = 2")),
                    );
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
                    first.error.is_some(),
                    "`web.fetch` resolves to nothing, so the parked cell ends in a link error"
                );
                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionDisposition::Accepted)
                    .await
                    .expect("settle the first returned cell");

                // The waiting cell, re-driven, now runs — on that same state.
                let second = state
                    .execute_code(admitted_context("waiting"), cell("second_cell = 2"))
                    .await
                    .expect("the cell that waited now runs");
                assert_eq!(second.error, None);
                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionDisposition::Accepted)
                    .await
                    .expect("settle the second returned cell");

                let total = state
                    .execute_code(admitted_context("total"), cell("finish second_cell"))
                    .await
                    .expect("execute code");
                assert_eq!(total.error, None);
                assert_eq!(total.terminal_finish, Some(serde_json::json!(2)));
            });
    }

    #[test]
    fn a_returned_cell_must_be_settled_before_another_cell_can_start() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = RlmRuntimeState::new_lashlang_for_tests().expect("runtime state");
                state
                    .execute_code(
                        lash_core::testing::code_execution_context(),
                        cell("first = 1"),
                    )
                    .await
                    .expect("first cell");

                let overlapping = state
                    .execute_code(
                        lash_core::testing::code_execution_context(),
                        cell("second = 2"),
                    )
                    .await
                    .expect_err("an unsettled response fences the next cell");
                assert!(matches!(
                    overlapping,
                    SessionError::Protocol(message)
                        if message == "the previous code execution response has not been settled"
                ));

                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionDisposition::Accepted)
                    .await
                    .expect("settle first response");
                state
                    .execute_code(
                        lash_core::testing::code_execution_context(),
                        cell("second = 2"),
                    )
                    .await
                    .expect("settlement releases the next cell");
            });
    }

    #[test]
    fn executing_code_refreshes_the_driver_bound_variables_snapshot() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = RlmRuntimeState::new_lashlang_for_tests().expect("runtime state");
                let prompt = state.shared_bound_variables_prompt();
                assert!(!prompt.read().expect("prompt read").contains("scratch_note"));

                state
                    .execute_code(
                        lash_core::testing::code_execution_context(),
                        lash_core::ExecRequest {
                            language: "lashlang".to_string(),
                            code: "scratch_note = \"after execution\"".to_string(),
                        },
                    )
                    .await
                    .expect("execute code");

                assert!(
                    prompt
                        .read()
                        .expect("prompt read")
                        .contains(r#"- `scratch_note` = "after execution""#)
                );
            });
    }

    #[test]
    fn cancelled_settlement_refreshes_the_driver_bound_variables_snapshot() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = RlmRuntimeState::new_lashlang_for_tests().expect("runtime state");
                let prompt = state.shared_bound_variables_prompt();

                state
                    .execute_code(
                        lash_core::testing::code_execution_context(),
                        cell("survives = 7"),
                    )
                    .await
                    .expect("execute accepted cell");
                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionDisposition::Accepted)
                    .await
                    .expect("accept first cell");
                state
                    .execute_code(
                        lash_core::testing::code_execution_context(),
                        cell("cancelled_tail = 1"),
                    )
                    .await
                    .expect("execute cell before late cancellation");
                assert!(
                    prompt
                        .read()
                        .expect("prompt read")
                        .contains("cancelled_tail")
                );

                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionDisposition::Cancelled)
                    .await
                    .expect("cancel second cell");
                let rendered = prompt.read().expect("prompt read");
                assert!(rendered.contains("survives"));
                assert!(!rendered.contains("cancelled_tail"));
            });
    }

    #[test]
    fn execute_code_uses_the_selected_typescript_dialect_and_rejects_unknown_languages() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = RlmRuntimeState::new_for_tests("typescript").expect("runtime state");
                let response = state
                    .execute_code(
                        lash_core::testing::code_execution_context(),
                        lash_core::ExecRequest {
                            language: "typescript".to_string(),
                            code: "const answer: number = 40 + 2; finish(answer);".to_string(),
                        },
                    )
                    .await
                    .expect("selected TypeScript dialect must execute");
                assert_eq!(response.error, None);
                assert_eq!(response.terminal_finish, Some(serde_json::json!(42)));

                let error = state
                    .execute_code(
                        lash_core::testing::code_execution_context(),
                        lash_core::ExecRequest {
                            language: "python".to_string(),
                            code: "finish(42)".to_string(),
                        },
                    )
                    .await
                    .expect_err("unregistered language must be rejected");

                assert!(matches!(
                    error,
                    SessionError::Protocol(message)
                        if message == "RLM language `python` is not registered"
                ));

                // The registered-but-inactive case is the one that matters for
                // dialect integrity: `lashlang` is a real, registered dialect,
                // and this session is pinned to `typescript`. Without this
                // fence a TypeScript session would execute a `<lashlang>` cell
                // — the cross-dialect violation `runbooks/RULES.md` treats as
                // an abort-and-RCA event. An earlier revision of this test
                // covered only the unregistered language, and deleting the
                // fence left the whole package green.
                let inactive = state
                    .execute_code(
                        lash_core::testing::code_execution_context(),
                        lash_core::ExecRequest {
                            language: "lashlang".to_string(),
                            code: "finish(42)".to_string(),
                        },
                    )
                    .await
                    .expect_err("a registered but inactive dialect must be rejected");

                assert!(
                    matches!(
                        &inactive,
                        SessionError::Protocol(message)
                            if message
                                == "RLM language `lashlang` is registered but session \
                                    language `typescript` is pinned"
                    ),
                    "{inactive:?}"
                );
            });
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

    /// One RLM seed event binding `projected_<label>` and defaulting the
    /// global `baton` to `label`.
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
        crate::projection::rlm_seed_initial_nodes(seed)
    }

    /// The value `finish baton` yields on the state's live execution.
    async fn live_baton(state: &RlmRuntimeState) -> serde_json::Value {
        let response = state
            .execute_code(
                lash_core::testing::code_execution_context(),
                cell("finish baton"),
            )
            .await
            .expect("the baton cell runs");
        state
            .settle_code_execution(lash_core::plugin::CodeExecutionDisposition::Accepted)
            .await
            .expect("settle the baton cell");
        assert_eq!(response.error, None);
        response.terminal_finish.expect("the baton cell finishes")
    }

    fn projected_seed_nodes(label: &str) -> Vec<lash_core::SessionAppendNode> {
        let mut seed = crate::projection::RlmSeed::default();
        seed.projected.push(
            format!("projected_{label}"),
            lash_rlm_types::RlmProjectedSeedEntry::Materialized(serde_json::json!({
                "value": label
            })),
        );
        crate::projection::rlm_seed_initial_nodes(seed)
    }

    fn projected_binding_names(contributions: &[lash_core::PromptContribution]) -> Vec<String> {
        let rendered = format!("{contributions:?}");
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
                let state = RlmRuntimeState::new_lashlang_for_tests().expect("state");
                state
                    .restore_runtime_session_state(seed_restore_view("frame-1", &["seed"]))
                    .await
                    .expect("first restore binds the seed");
                let first = state.projected_binding_prompt_contributions().await;
                assert_eq!(projected_binding_names(&first), ["projected_seed"]);

                state
                    .restore_runtime_session_state(seed_restore_view("frame-1", &["seed"]))
                    .await
                    .expect("a same-frame restore re-binds the seed it already holds");
                let second = state.projected_binding_prompt_contributions().await;
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
                let state = RlmRuntimeState::new_lashlang_for_tests().expect("state");
                state
                    .restore_runtime_session_state(seed_restore_view("frame-1", &["seed"]))
                    .await
                    .expect("restore binds the durable seed");
                let durable = state.projected_binding_prompt_contributions().await;

                state
                    .append_session_nodes(&projected_seed_nodes("discarded"))
                    .await
                    .expect("the append binds the pending seed");
                let pending = state.projected_binding_prompt_contributions().await;
                assert_eq!(
                    projected_binding_names(&pending),
                    ["projected_seed", "projected_discarded"]
                );

                state
                    .restore_runtime_session_state(seed_restore_view("frame-1", &["seed"]))
                    .await
                    .expect("rolling back to the durable view on the same frame succeeds");
                let rolled_back = state.projected_binding_prompt_contributions().await;
                assert_eq!(
                    format!("{rolled_back:?}"),
                    format!("{durable:?}"),
                    "the rolled-back append's binding must not survive the restore"
                );
            });
    }

    /// FIG-2521: a same-frame restore whose view carries no execution snapshot
    /// rebuilds the execution from the view's events on a fresh session, so a
    /// global assigned by a cell whose turn never committed does not survive.
    /// This is the view the runtime builds after a follow-on failure whose
    /// durable head holds no execution root (a frame switch clears it).
    #[test]
    fn same_frame_restore_without_a_snapshot_discards_uncommitted_execution() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let state = RlmRuntimeState::new_lashlang_for_tests().expect("state");
                state
                    .restore_runtime_session_state(restore_view(
                        "frame-1",
                        None,
                        baton_seed_nodes("committed"),
                    ))
                    .await
                    .expect("restore seeds the committed baton");
                assert_eq!(live_baton(&state).await, serde_json::json!("committed"));

                let mutated = state
                    .execute_code(
                        lash_core::testing::code_execution_context(),
                        cell("baton = \"uncommitted\"\nfinish baton"),
                    )
                    .await
                    .expect("the mutating cell runs");
                assert_eq!(
                    mutated.terminal_finish,
                    Some(serde_json::json!("uncommitted"))
                );
                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionDisposition::Accepted)
                    .await
                    .expect("settle the mutating cell");

                state
                    .restore_runtime_session_state(restore_view(
                        "frame-1",
                        None,
                        baton_seed_nodes("committed"),
                    ))
                    .await
                    .expect("a same-frame restore without a snapshot succeeds");
                assert_eq!(
                    live_baton(&state).await,
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
                let state = RlmRuntimeState::new_lashlang_for_tests().expect("state");
                state
                    .restore_runtime_session_state(restore_view(
                        "frame-1",
                        None,
                        baton_seed_nodes("committed"),
                    ))
                    .await
                    .expect("restore seeds the committed baton");
                let committed = state
                    .hydrated_execution_state()
                    .await
                    .expect("capture")
                    .expect("the RLM executor always holds a snapshotable state");

                let mutated = state
                    .execute_code(
                        lash_core::testing::code_execution_context(),
                        cell("baton = \"uncommitted\"\nfinish baton"),
                    )
                    .await
                    .expect("the mutating cell runs");
                assert_eq!(
                    mutated.terminal_finish,
                    Some(serde_json::json!("uncommitted"))
                );
                state
                    .settle_code_execution(lash_core::plugin::CodeExecutionDisposition::Accepted)
                    .await
                    .expect("settle the mutating cell");

                state
                    .restore_runtime_session_state(restore_view(
                        "frame-1",
                        Some(committed.clone()),
                        baton_seed_nodes("committed"),
                    ))
                    .await
                    .expect("a same-frame restore with a snapshot succeeds");
                assert_eq!(live_baton(&state).await, serde_json::json!("committed"));
                assert_eq!(
                    state
                        .hydrated_execution_state()
                        .await
                        .expect("capture")
                        .expect("state"),
                    committed,
                    "the restored execution is exactly the view's snapshot"
                );
            });
    }
}
