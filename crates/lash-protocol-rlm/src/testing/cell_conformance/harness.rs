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

const SEED: u64 = 0x0ce1_1c0f;

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
    /// The Restate server double every cell of the session runs over:
    /// lash-restate's engine and services over a SQLite memory store set,
    /// the twin the SQLite memory backend was (FIG-3668 B1). Each cell gets
    /// its own invocation, so no cell replays another.
    double: lash_restate_test::RestateTestBackend,
    /// The runtime the double's server tasks live on: held so every cell's
    /// `block_on` runs on the runtime the double was built with (FIG-3723).
    runtime: tokio::runtime::Runtime,
    /// Cells run so far, so a failure names the sequence that produced it.
    history: Vec<String>,
    /// The leaf bodies a host has been handed, by component key: what a
    /// rehydrating worker reads an unchanged leaf back from.
    stored_leaves: BTreeMap<String, Arc<[u8]>>,
    /// The host's read-only projected bindings, bound lazily through
    /// `projections` exactly as a host binds a projection it can re-resolve.
    /// Both belong to the host, so both outlive every restart.
    host_bindings: RlmProjectedBindings,
    projections: Arc<ProjectionRegistry>,
}

impl Session {
    pub(crate) fn open(mode: HarnessMode) -> Self {
        Self::open_with_host(mode, &BTreeMap::new())
    }

    /// A session whose host projects `host` as read-only bindings, each a lazy
    /// projection the host's registry resolves again after every restart.
    pub(crate) fn open_with_host(
        mode: HarnessMode,
        host: &BTreeMap<String, serde_json::Value>,
    ) -> Self {
        let projections = Arc::new(ProjectionRegistry::new());
        let mut host_bindings = RlmProjectedBindings::new();
        for (name, value) in host {
            let reference = projections.register_memory(Arc::new(HostJson(value.clone())));
            host_bindings = host_bindings
                .bind_lazy(name.clone(), reference)
                .expect("host binding names are unique");
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build the session runtime the double lives on");
        let double = runtime.block_on(crate::testing::kernel_double(
            SEED,
            lash_restate_test::ServerConfig::default(),
        ));
        Self {
            mode,
            state: RlmExecutionState::for_engine(LANGUAGE_ID),
            double,
            runtime,
            history: Vec::new(),
            stored_leaves: BTreeMap::new(),
            host_bindings,
            projections,
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
        let state = &mut self.state;
        let host_bindings = self.host_bindings.clone();
        let projections = Arc::clone(&self.projections);
        let artifact_store = lashlang::LashlangArtifacts::of_backend(&self.double.lash_backend());
        let cell = self.history.len();
        let double = &self.double;
        let response = self.runtime.block_on(async move {
            // The next cell's context: the double's ports under a handler and
            // an invocation of its own, numbered by the cells run so far. The
            // number is fixed width, so the persisted state the size laws
            // measure never moves with the cell count.
            let handler = double
                .open_handler(cell_scope())
                .await
                .expect("open the cell's handler");
            let context = lash_core::testing::code_execution_context_with_invocation(
                crate::testing::double_ports(double, &handler),
                lash_core::testing::exec_code_invocation(
                    "cell-conformance-session",
                    "cell-conformance-turn",
                    0,
                    cell,
                    format!("exec-code:{cell:08}"),
                    format!("exec-code:cell-conformance:{cell:08}"),
                ),
            );
            let response = execute_code_with_channel_and_bounds(
                state,
                context,
                request,
                artifact_store,
                LashlangSurface::default(),
                None,
                host_bindings,
                projections,
                RlmLashlangExecutionTraceConfig::default(),
                lashlang::ExecutionBounds::unbounded(),
                crate::plugin::RlmChannel::Cell,
            )
            .await;
            handler.close().await.expect("close the cell's handler");
            response
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
    /// production path: the same incremental capture the runtime commits after
    /// a turn — a changed leaf's body, an unchanged leaf by reference to what
    /// the host already holds — hydrated back through the same restore a
    /// rehydrating worker uses. A capture that forgot a change would restore
    /// the stale leaf here exactly as it would in production.
    pub(crate) fn restart(&mut self) {
        let snapshot = self
            .state
            .snapshot_execution_state(lash_core::FleetFormat::current())
            .expect("capture the RLM execution state");
        self.state.acknowledge_execution_state_capture();
        let mut components = BTreeMap::new();
        for (key, component) in snapshot.components {
            let body = match component {
                lash_core::plugin::ExecutionStateComponentSnapshot::Changed(body) => {
                    self.stored_leaves.insert(key.clone(), Arc::clone(&body));
                    body
                }
                lash_core::plugin::ExecutionStateComponentSnapshot::Unchanged => {
                    self.stored_leaves.get(&key).cloned().unwrap_or_else(|| {
                        panic!("an unchanged leaf `{key}` the host never stored")
                    })
                }
            };
            components.insert(key, body);
        }
        let hydrated = lash_core::plugin::HydratedExecutionState {
            root: snapshot.root.expect("a capture carries its root"),
            components,
        };
        let mut restored = RlmExecutionState::for_engine(LANGUAGE_ID);
        restored
            .restore_execution_state(&hydrated, lash_core::FleetFormat::current())
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
            &self.state.opaque_bound_variables(&none),
            crate::dialect::DialectPromptVocabulary::default(),
            &crate::render::BuiltinCodeRenderer,
            &lash_render::RenderParams::preview(),
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
        self.state
            .hydrated_execution_state(lash_core::FleetFormat::current())
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
        let double = &self.double;
        let evidence = self
            .runtime
            .block_on(async {
                // A parked cell runs on a handler of its own, as it ran on a
                // host of its own: its context carries no per-cell invocation.
                let handler = double
                    .open_handler(crate::testing::default_cell_scope())
                    .await
                    .expect("open the parked cell's handler");
                let evidence = Box::pin(execute_parked_cell_for_tests(
                    &mut state,
                    crate::executor::parked_cell_context_for_tests(crate::testing::double_ports(
                        double, &handler,
                    )),
                    LANGUAGE_ID,
                    code,
                    false,
                ))
                .await;
                handler
                    .close()
                    .await
                    .expect("close the parked cell's handler");
                evidence
            })
            .unwrap_or_else(|error| {
                panic!("parked cell `{code}` must suspend and resume: {error}")
            });
        self.state = state;
        self.history.push(code.to_string());
        evidence
    }

    /// Injects the retention defect used by the red-proof law. The broken
    /// continuation must fail before it can produce a terminal value.
    pub(crate) fn run_parked_broken(&mut self, code: &str) -> String {
        let mut state =
            std::mem::replace(&mut self.state, RlmExecutionState::for_engine(LANGUAGE_ID));
        let double = &self.double;
        let result = self.runtime.block_on(async {
            // A parked cell runs on a handler of its own, as it ran on a host
            // of its own: its context carries no per-cell invocation.
            let handler = double
                .open_handler(crate::testing::default_cell_scope())
                .await
                .expect("open the parked cell's handler");
            let result = Box::pin(execute_parked_cell_for_tests(
                &mut state,
                crate::executor::parked_cell_context_for_tests(crate::testing::double_ports(
                    double, &handler,
                )),
                LANGUAGE_ID,
                code,
                true,
            ))
            .await;
            handler
                .close()
                .await
                .expect("close the parked cell's handler");
            result
        });
        self.state = state;
        result.expect_err("the deliberately broken continuation must fail")
    }
}

/// A host's read-only JSON value, projected: it answers materialization and
/// the structural reads a host document answers (a field, an index, its keys
/// and length); every other read falls back to materializing.
struct HostJson(serde_json::Value);

impl lashlang::ProjectedHostDescriptor for HostJson {
    fn type_name(&self) -> &str {
        match &self.0 {
            serde_json::Value::Null => "null",
            serde_json::Value::Bool(_) => "boolean",
            serde_json::Value::Number(_) => "number",
            serde_json::Value::String(_) => "string",
            serde_json::Value::Array(_) => "array",
            serde_json::Value::Object(_) => "object",
        }
    }

    fn read_one(
        &self,
        request: lashlang::ProjectedReadRequest,
    ) -> Option<lashlang::ProjectedReadResponse> {
        use lashlang::{ProjectedReadRequest as Read, ProjectedReadResponse as Answer};
        let json = |value: Option<&serde_json::Value>| {
            Answer::Value(
                value
                    .cloned()
                    .map_or(lashlang::Value::Undefined, lashlang::from_json),
            )
        };
        match (request, &self.0) {
            (Read::Materialize, value) => Some(json(Some(value))),
            (Read::Field(name), serde_json::Value::Object(fields)) => {
                Some(json(fields.get(name.as_ref())))
            }
            (Read::Index(lashlang::Value::Number(index)), serde_json::Value::Array(items)) => {
                Some(json(items.get(index as usize)))
            }
            (Read::Keys, serde_json::Value::Object(fields)) => {
                Some(Answer::Keys(fields.keys().cloned().collect()))
            }
            (Read::Len, serde_json::Value::Array(items)) => Some(Answer::Len(items.len())),
            _ => None,
        }
    }
}
