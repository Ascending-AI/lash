//! The session harness the cell-conformance scenarios drive.
//!
//! One [`Session`] is one RLM session: a sequence of cells, each compiled on
//! its own against the session's surviving execution state, exactly as the
//! protocol runs them. The harness owns the two things the scenarios must not
//! re-derive — how a cell is executed, and what "the session
//! restarted" means — so a scenario reads as the cell sequence it is.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core::ExecRequest;
use lash_lashlang_runtime::LashlangSurface;

use crate::executor::{
    ParkedCellEvidence, RlmExecutionState, RlmLashlangExecutionTraceConfig,
    execute_code_with_channel_and_bounds, execute_parked_cell_for_tests,
};
use crate::projection::{ProjectionRegistry, RlmProjectedBindings, flow_to_json_value};

/// The one language an RLM session runs (ADR 0096).
pub(crate) const LANGUAGE_ID: &str = crate::dialect::typescript::LANGUAGE_ID;

/// How much of the session survives between two cells.
///
/// [`HarnessMode::Resident`] is the hot path: one live `RlmExecutionState` for
/// the whole session. [`HarnessMode::RestartBetweenCells`] is the durability
/// path: every cell boundary goes through the same snapshot and restore a host
/// performs when the session is rehydrated on another worker. Running the same
/// scenario under both is the point — a cross-cell law that only holds while
/// the process stays up is not a law of the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HarnessMode {
    Resident,
    RestartBetweenCells,
}

impl HarnessMode {
    pub(crate) const ALL: &'static [HarnessMode] =
        &[HarnessMode::Resident, HarnessMode::RestartBetweenCells];
}

/// What a single cell did, reduced to what the cross-cell laws talk about.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CellOutcome {
    /// The cell's typed failure, if it failed. Every failing cell must produce
    /// one: a cell that neither runs nor fails typed is the shape the
    /// no-poisoning law forbids.
    pub(crate) error: Option<lash_core::CellFailure>,
    /// The terminal value the cell finished the session with, if it did.
    pub(crate) finish: Option<serde_json::Value>,
}

impl CellOutcome {
    pub(crate) fn succeeded(&self) -> bool {
        self.error.is_none()
    }

    /// The failure text, for a cell the scenario expects to fail.
    pub(crate) fn failure(&self) -> &str {
        self.error
            .as_ref()
            .map(|failure| failure.message.as_str())
            .expect("a cell expected to fail reported no error")
    }
}

/// One RLM session under test.
pub(crate) struct Session {
    mode: HarnessMode,
    state: RlmExecutionState,
    /// The SQLite memory backend (ADR 0102) every cell of the session runs
    /// over; each cell gets its own invocation, so no cell replays another.
    backend: lash_sqlite_store::SqliteBackend,
    /// Cells run so far, so a failure names the sequence that produced it.
    history: Vec<String>,
}

impl Session {
    pub(crate) fn open(mode: HarnessMode) -> Self {
        Self {
            mode,
            state: RlmExecutionState::for_engine(LANGUAGE_ID),
            backend: block_on(lash_sqlite_store::SqliteBackend::memory())
                .expect("open a memory backend"),
            history: Vec::new(),
        }
    }

    /// A cell that fails is a normal outcome here: the no-poisoning law is
    /// about what the *next* cell sees, so a scenario has to be able to run a
    /// failing cell without the harness deciding that is a test failure.
    pub(crate) fn run(&mut self, code: &str) -> CellOutcome {
        let response = self.run_observed(code);
        CellOutcome {
            error: response.error,
            finish: response.terminal_finish,
        }
    }

    /// Runs a cell and returns everything it reported: its printed
    /// observations as well as its failure and terminal value.
    pub(crate) fn run_observed(&mut self, code: &str) -> lash_core::ExecResponse {
        let request = ExecRequest {
            language: LANGUAGE_ID.to_string(),
            code: code.to_string(),
        };
        let context = self.cell_context();
        let state = &mut self.state;
        let response = block_on(async move {
            execute_code_with_channel_and_bounds(
                state,
                context,
                request,
                lashlang::global_in_memory_lashlang_artifact_store(),
                LashlangSurface::default(),
                None,
                RlmProjectedBindings::default(),
                Arc::new(ProjectionRegistry::new()),
                RlmLashlangExecutionTraceConfig::default(),
                lashlang::ExecutionBounds::unbounded(),
                crate::plugin::RlmChannel::Cell,
            )
            .await
        });
        self.history.push(code.to_string());
        if self.mode == HarnessMode::RestartBetweenCells {
            self.restart();
        }
        response
    }

    /// Runs a cell that the scenario requires to succeed.
    pub(crate) fn run_ok(&mut self, code: &str) -> CellOutcome {
        let outcome = self.run(code);
        assert!(
            outcome.succeeded(),
            "cell `{code}` must succeed after {:?}: {:?}",
            self.history,
            outcome.error
        );
        outcome
    }

    /// Runs a cell that the scenario requires to fail, and returns its typed
    /// failure text.
    pub(crate) fn run_failing(&mut self, code: &str) -> String {
        let outcome = self.run(code);
        assert!(
            !outcome.succeeded(),
            "cell `{code}` was expected to fail but succeeded"
        );
        outcome.failure().to_string()
    }

    /// Snapshots the session and restores it into a fresh engine, discarding
    /// everything a live process was holding.
    ///
    /// This is the harness's whole model of a restart, and it is the
    /// production path: the same capture the runtime persists, hydrated back
    /// through the same restore a rehydrating worker uses.
    /// The next cell's context: the session's backend under an invocation
    /// of its own, numbered by the cells run so far. The number is fixed
    /// width, so the persisted state the size laws measure never moves with
    /// the cell count.
    fn cell_context(&self) -> lash_core::RuntimeExecutionContext<'static> {
        let cell = self.history.len();
        lash_core::testing::code_execution_context_with_invocation(
            &self.backend,
            lash_core::testing::exec_code_invocation(
                "cell-conformance-session",
                "cell-conformance-turn",
                0,
                cell,
                format!("exec-code:{cell:08}"),
                format!("exec-code:cell-conformance:{cell:08}"),
            ),
        )
    }

    pub(crate) fn restart(&mut self) {
        let hydrated = self
            .state
            .hydrated_execution_state()
            .expect("capture the RLM execution state");
        let mut restored = RlmExecutionState::for_engine(LANGUAGE_ID);
        restored
            .restore_execution_state(&hydrated)
            .expect("restore the RLM execution state");
        self.state = restored;
    }

    /// The session's bindings as a host sees them.
    ///
    /// This is the materialized view, not the runtime's internal roots, which
    /// is exactly the surface the cross-cell laws are stated over: what a later
    /// cell and a reading host can both observe.
    pub(crate) fn globals(&self) -> BTreeMap<String, serde_json::Value> {
        let bindings = self
            .state
            .bound_variable_values(&std::collections::BTreeSet::new());
        block_on(async move {
            let mut out = BTreeMap::new();
            for (name, value) in bindings {
                out.insert(name, flow_to_json_value(&value).await);
            }
            out
        })
    }

    /// The names the next cell links against: the session's live globals.
    pub(crate) fn global_names(&self) -> std::collections::BTreeSet<String> {
        self.state
            .bound_variable_values(&std::collections::BTreeSet::new())
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    /// The session's persisted execution state: the root record and every leaf
    /// body, exactly as a host would store them.
    pub(crate) fn persisted_state(&self) -> lash_core::plugin::HydratedExecutionState {
        self.state
            .hydrated_execution_state()
            .expect("capture the RLM execution state")
    }

    /// The size of the session's persisted execution state, in bytes.
    ///
    /// The leak regression is stated over this rather than over heap internals on purpose: an
    /// unbounded heap that never reaches the wire costs a host nothing, and a bounded heap
    /// that writes an unbounded snapshot costs it everything.
    pub(crate) fn persisted_bytes(&self) -> usize {
        let hydrated = self.persisted_state();
        hydrated.root.len() + hydrated.components.values().map(|v| v.len()).sum::<usize>()
    }

    /// Runs a cell through the VM's process-mode effect boundary, snapshots
    /// the real continuation, restores it, and resumes to completion. This is
    /// deliberately test-only: foreground cells still use the production RLM
    /// executor above, while this method supplies the missing continuation
    /// composition without adding a production suspension policy.
    pub(crate) fn run_parked(&mut self, code: &str) -> ParkedCellEvidence {
        let mut state =
            std::mem::replace(&mut self.state, RlmExecutionState::for_engine(LANGUAGE_ID));
        let evidence = block_on(async {
            // A parked cell runs on a backend of its own, as it ran on a host
            // of its own: its context carries no per-cell invocation.
            let backend = lash_sqlite_store::SqliteBackend::memory()
                .await
                .expect("open a memory backend");
            Box::pin(execute_parked_cell_for_tests(
                &mut state,
                crate::executor::parked_cell_context_for_tests(&backend),
                LANGUAGE_ID,
                code,
                false,
            ))
            .await
        })
        .unwrap_or_else(|error| panic!("parked cell `{code}` must suspend and resume: {error}"));
        self.state = state;
        self.history.push(code.to_string());
        evidence
    }

    /// Injects the retention defect used by the red-proof law. The broken
    /// continuation must fail before it can produce a terminal value.
    pub(crate) fn run_parked_broken(&mut self, code: &str) -> String {
        let mut state =
            std::mem::replace(&mut self.state, RlmExecutionState::for_engine(LANGUAGE_ID));
        let result = block_on(async {
            // A parked cell runs on a backend of its own, as it ran on a host
            // of its own: its context carries no per-cell invocation.
            let backend = lash_sqlite_store::SqliteBackend::memory()
                .await
                .expect("open a memory backend");
            Box::pin(execute_parked_cell_for_tests(
                &mut state,
                crate::executor::parked_cell_context_for_tests(&backend),
                LANGUAGE_ID,
                code,
                true,
            ))
            .await
        });
        self.state = state;
        result.expect_err("the deliberately broken continuation must fail")
    }
}

pub(crate) fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build a current-thread runtime")
        .block_on(future)
}
