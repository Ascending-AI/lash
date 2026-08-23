//! The session harness the cell-conformance scenarios drive.
//!
//! One [`Session`] is one RLM session: a sequence of cells, each compiled on
//! its own against the session's surviving execution state, exactly as the
//! protocol runs them. The harness owns the two things the scenarios must not
//! re-derive — how a cell is executed for a dialect, and what "the session
//! restarted" means — so a scenario reads as the cell sequence it is.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core::ExecRequest;
use lash_lashlang_runtime::LashlangSurface;

use crate::executor::{
    ParkedCellEvidence, RlmExecutionState, RlmLashlangExecutionTraceConfig, SourceDialect,
    execute_code_with_dialect_and_bounds, execute_parked_cell_for_tests,
};
use crate::projection::{ProjectionRegistry, RlmProjectedBindings, flow_to_json_value};

/// The two source dialects an RLM session can be opened in.
///
/// The same value the executor runs a cell with, so the harness drives the
/// production path for a dialect rather than choosing between two of them.
pub(crate) type Dialect = SourceDialect;

impl Dialect {
    /// Every dialect the conformance suite covers, in a stable order.
    pub(crate) const ALL: &'static [Dialect] = &[Dialect::Lashlang, Dialect::Typescript];
}

impl std::fmt::Display for Dialect {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.language_id())
    }
}

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
    pub(crate) error: Option<String>,
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
            .as_deref()
            .expect("a cell expected to fail reported no error")
    }
}

/// One RLM session under test.
pub(crate) struct Session {
    dialect: Dialect,
    mode: HarnessMode,
    state: RlmExecutionState,
    /// Cells run so far, so a failure names the sequence that produced it.
    history: Vec<String>,
}

impl Session {
    pub(crate) fn open(dialect: Dialect, mode: HarnessMode) -> Self {
        Self {
            dialect,
            mode,
            state: RlmExecutionState::for_engine(dialect.language_id()),
            history: Vec::new(),
        }
    }

    /// Runs one cell and returns its outcome, whatever it is.
    ///
    /// A cell that fails is a normal outcome here: the no-poisoning law is
    /// about what the *next* cell sees, so a scenario has to be able to run a
    /// failing cell without the harness deciding that is a test failure.
    pub(crate) fn run(&mut self, code: &str) -> CellOutcome {
        let request = ExecRequest {
            language: self.dialect.language_id().to_string(),
            code: code.to_string(),
            accept_finish: true,
        };
        let dialect = self.dialect;
        let state = &mut self.state;
        let response = block_on(async move {
            execute_code_with_dialect_and_bounds(
                state,
                lash_core::testing::code_execution_context(),
                request,
                lashlang::global_in_memory_lashlang_artifact_store(),
                LashlangSurface::default(),
                None,
                RlmProjectedBindings::default(),
                Arc::new(ProjectionRegistry::new()),
                RlmLashlangExecutionTraceConfig::default(),
                lashlang::ExecutionBounds::unbounded(),
                dialect,
            )
            .await
        });
        self.history.push(code.to_string());
        if self.mode == HarnessMode::RestartBetweenCells {
            self.restart();
        }
        CellOutcome {
            error: response.error,
            finish: response.terminal_finish,
        }
    }

    /// Runs a cell that the scenario requires to succeed.
    pub(crate) fn run_ok(&mut self, code: &str) -> CellOutcome {
        let outcome = self.run(code);
        assert!(
            outcome.succeeded(),
            "cell `{code}` must succeed in {} after {:?}: {:?}",
            self.dialect,
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
            "cell `{code}` was expected to fail in {} but succeeded",
            self.dialect
        );
        outcome.failure().to_string()
    }

    /// Snapshots the session and restores it into a fresh engine, discarding
    /// everything a live process was holding.
    ///
    /// This is the harness's whole model of a restart, and it is the
    /// production path: the same capture the runtime persists, hydrated back
    /// through the same restore a rehydrating worker uses.
    pub(crate) fn restart(&mut self) {
        let hydrated = self
            .state
            .hydrated_execution_state()
            .expect("capture the RLM execution state");
        let mut restored = RlmExecutionState::for_engine(self.dialect.language_id());
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

    /// The session's user-visible bindings.
    ///
    /// Excludes the TypeScript lowering's `__typescript_<n>_callback_receiver`
    /// scratch binding. It is an artefact of how an array callback is lowered
    /// rather than something a cell bound, and it keeps the previous
    /// callback's receiver until the next one overwrites it, so a law stated
    /// over "what the cells bound" must not read it. [`Session::globals`]
    /// keeps the unfiltered view for anything that needs it.
    pub(crate) fn user_bindings(&self) -> BTreeMap<String, serde_json::Value> {
        self.globals()
            .into_iter()
            .filter(|(name, _)| !name.starts_with("__typescript_"))
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
    /// The leak regression is stated over this rather than over heap internals
    /// on purpose: an unbounded heap that never reaches the wire costs a host
    /// nothing, and a bounded heap that writes an unbounded snapshot costs it
    /// everything. This is the number that gets stored per turn.
    pub(crate) fn persisted_bytes(&self) -> usize {
        let hydrated = self.persisted_state();
        hydrated.root.len() + hydrated.components.values().map(Vec::len).sum::<usize>()
    }

    /// Runs a cell through the VM's process-mode effect boundary, snapshots
    /// the real continuation, restores it, and resumes to completion. This is
    /// deliberately test-only: foreground cells still use the production RLM
    /// executor above, while this method supplies the missing continuation
    /// composition without adding a production suspension policy.
    pub(crate) fn run_parked(&mut self, code: &str) -> ParkedCellEvidence {
        let mut state = std::mem::replace(
            &mut self.state,
            RlmExecutionState::for_engine(self.dialect.language_id()),
        );
        let evidence = block_on(execute_parked_cell_for_tests(
            &mut state,
            crate::executor::parked_cell_context_for_tests(),
            self.dialect.language_id(),
            code,
            false,
        ))
        .unwrap_or_else(|error| {
            panic!(
                "parked cell `{code}` must suspend and resume in {}: {error}",
                self.dialect
            )
        });
        self.state = state;
        self.history.push(code.to_string());
        evidence
    }

    /// Injects the retention defect used by the red-proof law. The broken
    /// continuation must fail before it can produce a terminal value.
    pub(crate) fn run_parked_broken(&mut self, code: &str) -> String {
        let mut state = std::mem::replace(
            &mut self.state,
            RlmExecutionState::for_engine(self.dialect.language_id()),
        );
        let result = block_on(execute_parked_cell_for_tests(
            &mut state,
            crate::executor::parked_cell_context_for_tests(),
            self.dialect.language_id(),
            code,
            true,
        ));
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
