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
    RlmExecutionState, RlmLashlangExecutionTraceConfig, execute_code_with_bounds,
    execute_typescript_code_with_bounds,
};
use crate::projection::{ProjectionRegistry, RlmProjectedBindings, flow_to_json_value};

/// The two source dialects an RLM session can be opened in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Dialect {
    Lashlang,
    Typescript,
}

impl Dialect {
    /// Every dialect the conformance suite covers, in a stable order.
    pub(crate) const ALL: &'static [Dialect] = &[Dialect::Lashlang, Dialect::Typescript];

    pub(crate) fn language_id(self) -> &'static str {
        match self {
            Self::Lashlang => "lashlang",
            Self::Typescript => "typescript",
        }
    }
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
    state: Option<RlmExecutionState>,
    /// Cells run so far, so a failure names the sequence that produced it.
    history: Vec<String>,
}

impl Session {
    pub(crate) fn open(dialect: Dialect, mode: HarnessMode) -> Self {
        Self {
            dialect,
            mode,
            state: Some(
                RlmExecutionState::for_engine(dialect.language_id())
                    .expect("open an RLM execution state"),
            ),
            history: Vec::new(),
        }
    }

    /// Runs one cell and returns its outcome, whatever it is.
    ///
    /// A cell that fails is a normal outcome here: the no-poisoning law is
    /// about what the *next* cell sees, so a scenario has to be able to run a
    /// failing cell without the harness deciding that is a test failure.
    pub(crate) fn run(&mut self, code: &str) -> CellOutcome {
        let state = self.state.take().expect("session state is resident");
        let request = ExecRequest {
            language: self.dialect.language_id().to_string(),
            code: code.to_string(),
            accept_finish: true,
        };
        let dialect = self.dialect;
        let (state, response) = block_on(async move {
            let context = lash_core::testing::code_execution_context();
            let artifact_store = lashlang::global_in_memory_lashlang_artifact_store();
            match dialect {
                Dialect::Lashlang => {
                    execute_code_with_bounds(
                        state,
                        context,
                        request,
                        artifact_store,
                        LashlangSurface::default(),
                        None,
                        RlmProjectedBindings::default(),
                        Arc::new(ProjectionRegistry::new()),
                        RlmLashlangExecutionTraceConfig::default(),
                        lashlang::ExecutionBounds::unbounded(),
                    )
                    .await
                }
                Dialect::Typescript => {
                    execute_typescript_code_with_bounds(
                        state,
                        context,
                        request,
                        artifact_store,
                        LashlangSurface::default(),
                        None,
                        RlmProjectedBindings::default(),
                        Arc::new(ProjectionRegistry::new()),
                        RlmLashlangExecutionTraceConfig::default(),
                        lashlang::ExecutionBounds::unbounded(),
                    )
                    .await
                }
            }
        })
        .expect("the executor answers every cell");
        self.state = Some(state);
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
        let state = self.state.take().expect("session state is resident");
        let hydrated = state
            .hydrated_execution_state()
            .expect("capture the RLM execution state");
        let mut restored = RlmExecutionState::for_engine(self.dialect.language_id())
            .expect("open a replacement RLM execution state");
        restored
            .restore_execution_state(&hydrated)
            .expect("restore the RLM execution state");
        self.state = Some(restored);
    }

    /// The session's bindings as a host sees them.
    ///
    /// This is the materialized view, not the runtime's internal roots, which
    /// is exactly the surface the cross-cell laws are stated over: what a later
    /// cell and a reading host can both observe.
    pub(crate) fn globals(&self) -> BTreeMap<String, serde_json::Value> {
        let state = self.state.as_ref().expect("session state is resident");
        let bindings = state.bound_variable_values(&std::collections::BTreeSet::new());
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
        let state = self.state.as_ref().expect("session state is resident");
        state
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
}

pub(crate) fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build a current-thread runtime")
        .block_on(future)
}
