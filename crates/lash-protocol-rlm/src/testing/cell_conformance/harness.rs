//! The session harness the cell-conformance scenarios shift.
//!
//! One [`Session`] is one RLM session: a sequence of cells, each compiled on
//! its own against the session's surviving execution state, exactly as the
//! protocol runs them. The harness owns the two things the scenarios must not
//! re-derive — how a cell is executed, and what "the session
//! restarted" means — so a scenario reads as the cell sequence it is.
//!
//! Every cell runs on the durable path: under the claimed context of the
//! session's actor on a SQLite memory store set ([`DurableHost`]), its
//! snapshots and admissions committed through the claim. A restart is a node
//! kill followed by resume on another node, with the session's execution
//! state captured and restored through the production capture.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core::ExecRequest;
use lash_lashlang_runtime::LashlangSurface;

use crate::executor::RlmExecutionState;
use crate::projection::{RlmProjectedBindings, flow_to_json_value};
use crate::testing::{DurableHost, execute_code_with_channel_and_bounds};

/// The one language an RLM session runs (ADR 0096).
pub(crate) const LANGUAGE_ID: &str = crate::dialect::typescript::LANGUAGE_ID;

/// The scope every cell of a conformance session claims: one turn of one
/// session, matching the `exec_code_invocation` each cell installs.
fn cell_scope() -> lash_core::AdmittedScope {
    lash_core::AdmittedScope::turn(
        lash_core::SessionId::from("cell-conformance-session"),
        lash_core::TurnId::from("cell-conformance-turn"),
    )
}

/// How much of the session survives between two cells.
///
/// [`HarnessMode::Resident`] is the hot path: one live `RlmExecutionState` for
/// the whole session. [`HarnessMode::RestartBetweenCells`] is the durability
/// path: every cell boundary goes through the same snapshot and restore a host
/// performs when the session is rehydrated on another node, and the node
/// serving the session dies. Running the same
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
    workers: lash_vm_client::service::Service,
    /// The session's actor on the durable store, claimed by a node: every
    /// cell runs under its context, each under its own invocation, so no
    /// cell's snapshot is another's.
    host: DurableHost,
    /// The runtime every cell's `block_on` runs on.
    runtime: tokio::runtime::Runtime,
    /// Cells run so far, so a failure names the sequence that produced it.
    history: Vec<String>,
    /// The leaf bodies a host has been handed, by component key: what a
    /// rehydrating worker reads an unchanged leaf back from.
    stored_leaves: BTreeMap<lash_core::plugin::ExecutionLeafName, Arc<[u8]>>,
    /// The host's read-only projected bindings, re-supplied to every cell.
    /// They belong to the host, so they outlive every restart.
    host_bindings: RlmProjectedBindings,
}

impl Session {
    pub(crate) fn open(mode: HarnessMode) -> Self {
        Self::open_with_host(mode, &BTreeMap::new())
    }

    /// A session whose host projects `host` as read-only bindings, bound
    /// again for every cell the way a host re-supplies them after a restart.
    pub(crate) fn open_with_host(
        mode: HarnessMode,
        host: &BTreeMap<String, serde_json::Value>,
    ) -> Self {
        Self::open_with_workers(mode, host, lash_vm_client::service::Service::default())
    }

    pub(crate) fn open_with_workers(
        mode: HarnessMode,
        host: &BTreeMap<String, serde_json::Value>,
        workers: lash_vm_client::service::Service,
    ) -> Self {
        let mut host_bindings = RlmProjectedBindings::new();
        for (name, value) in host {
            host_bindings = host_bindings
                .bind_json(name.clone(), value.clone())
                .expect("host binding names are unique");
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build the session runtime");
        let host = runtime.block_on(DurableHost::open(cell_scope()));
        Self {
            mode,
            state: RlmExecutionState::for_engine_with_workers(LANGUAGE_ID, workers.clone()),
            workers,
            host,
            runtime,
            history: Vec::new(),
            stored_leaves: BTreeMap::new(),
            host_bindings,
        }
    }

    /// The names of the host's projected bindings.
    pub(crate) fn host_binding_names(&self) -> std::collections::BTreeSet<String> {
        self.host_bindings.names().collect()
    }

    /// A cell that fails is a normal outcome here: the no-poisoning law is
    /// about what the *next* cell sees, so a scenario has to be able to run a
    /// failing cell without the harness deciding that is a test failure.
    pub(crate) fn run(&mut self, code: &str) -> CellOutcome {
        let response = self.run_observed(code);
        CellOutcome {
            error: response.error().cloned(),
            finish: response.finish_value().cloned(),
        }
    }

    /// Runs a cell and returns everything it reported: its printed
    /// observations as well as its failure and terminal value.
    pub(crate) fn run_observed(&mut self, code: &str) -> lash_core::ExecResponse {
        let request = ExecRequest {
            code: code.to_string(),
        };
        let state = &mut self.state;
        let host_bindings = self.host_bindings.clone();
        let artifact_store = lashlang::LashlangArtifacts::of_backend(self.host.backend());
        let cell = self.history.len();
        let ports = self.host.ports();
        let response = self.runtime.block_on(async move {
            // The next cell's context: the host's ports under the claimed
            // context and an invocation of its own, numbered by the cells run
            // so far. The number is fixed width, so the persisted state the
            // size laws measure never moves with the cell count.
            let context = lash_core::testing::code_execution_context_with_invocation(
                ports,
                lash_core::testing::exec_code_invocation(
                    "cell-conformance-session",
                    "cell-conformance-turn",
                    0,
                    cell,
                    format!("exec-code:{cell:08}"),
                    format!("exec-code:cell-conformance:{cell:08}"),
                ),
            );
            execute_code_with_channel_and_bounds(
                state,
                context.with_recorded_render(crate::testing::recorded_test_render()),
                request,
                artifact_store,
                LashlangSurface::default(),
                None,
                host_bindings,
                None,
                lashlang::ExecutionBounds::unbounded(),
                crate::plugin::RlmChannel::Cell,
                crate::render::CodeRendererSlot::default(),
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

    /// Snapshots the session and restores it into a fresh engine on another
    /// node, discarding everything a live process was holding.
    ///
    /// This is the harness's whole model of a restart, and it is the
    /// production path: the same incremental capture the runtime commits after
    /// a turn — a changed leaf's body, an unchanged leaf by reference to what
    /// the host already holds — hydrated back through the same restore a
    /// rehydrating worker uses. A capture that forgot a change would restore
    /// the stale leaf here exactly as it would in production.
    pub(crate) fn restart(&mut self) {
        let snapshot = self
            .runtime
            .block_on(
                self.state
                    .snapshot_execution_state(lash_core::FleetFormat::current()),
            )
            .expect("capture the RLM execution state");
        self.state.acknowledge_execution_state_capture();
        let lash_core::plugin::ExecutionStateCapture::Replace { root, leaves } = snapshot else {
            panic!("expected replacement capture");
        };
        let mut components = BTreeMap::new();
        for (key, component) in leaves {
            let body = match component {
                lash_core::plugin::LeafChange::Changed(body) => {
                    self.stored_leaves.insert(key.clone(), Arc::clone(&body));
                    body
                }
                lash_core::plugin::LeafChange::Unchanged => {
                    self.stored_leaves.get(&key).cloned().unwrap_or_else(|| {
                        panic!("an unchanged leaf `{key}` the host never stored")
                    })
                }
            };
            components.insert(key, body);
        }
        let hydrated = lash_core::plugin::HydratedExecutionState { root, components };
        let mut restored =
            RlmExecutionState::for_engine_with_workers(LANGUAGE_ID, self.workers.clone());
        self.runtime
            .block_on(
                restored.restore_execution_state(&hydrated, lash_core::FleetFormat::current()),
            )
            .expect("restore the RLM execution state");
        self.state = restored;
        self.runtime.block_on(self.host.kill_and_resume());
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
        {
            let mut out = BTreeMap::new();
            for (name, value) in bindings {
                out.insert(name, flow_to_json_value(&value));
            }
            out
        }
    }

    /// The prompt's "Bound Variables" section for the session as it stands,
    /// rendered by the production renderer from the production inputs.
    pub(crate) fn bound_variables_prompt(&self) -> String {
        let none = std::collections::BTreeSet::new();
        crate::rlm_support::render_bound_variables(
            &mut crate::rlm_support::BoundVariableRenderCache::default(),
            &self.state.bound_variable_values(&none),
            &self
                .runtime
                .block_on(
                    self.state
                        .opaque_bound_variables(&none, &lashlang::BindingSummaryConfig::standard()),
                )
                .expect("opaque summaries"),
            &crate::dialect::TypescriptDialect,
            &crate::render::BuiltinCodeRenderer,
            &lash_render::RenderParams::preview(),
            crate::RlmPresentationConfig::standard().max_inline_keys,
        )
        .to_string()
    }

    /// The names the next cell links against: the session's live globals.
    pub(crate) fn global_names(&self) -> std::collections::BTreeSet<String> {
        self.state
            .binding_names()
            .filter(|name| *name != "history")
            .map(str::to_string)
            .collect()
    }

    /// The globals a cell boundary dropped for holding a function, which the
    /// next cell links against as refused names.
    pub(crate) fn expired_functions(&self) -> std::collections::BTreeSet<String> {
        self.state.expired_functions().clone()
    }

    /// The session's persisted execution state: the root record and every leaf
    /// body, exactly as a host would store them.
    pub(crate) fn persisted_state(&self) -> lash_core::plugin::HydratedExecutionState {
        self.runtime
            .block_on(
                self.state
                    .hydrated_execution_state(lash_core::FleetFormat::current()),
            )
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
}
