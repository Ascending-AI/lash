/// Derives a durable frame-node identity through the runtime's canonical
/// producer for integration fixtures that need to enqueue frame-scoped work.
pub use lash_core::facade_support::frame_node_id;
/// Prompt composition over a cut a test states, outside any model call: the
/// runtime builds a call's cut and composes it only at the call's admission
/// (ADR 0133).
pub use lash_core::testing::prompt;
pub use lash_core::testing::run_tool;
/// Runs one granted tool call with mock contexts, so a provider's granted
/// branch is exercisable outside a live turn.
pub use lash_core::testing::run_tool_granted;
/// One backend with some of its ports decorated by a test that observes or
/// faults them: the decorated backend is still one substrate (ADR 0104, B2).
pub use lash_core::testing::runtime_helpers::LayeredBackend;
/// A standalone [`ToolRegistry`](crate::tools::ToolRegistry) plus the
/// [`ToolSourceHandle`](crate::tools::ToolSourceHandle) `provider` registered
/// under — the live-source route a run's built registry takes, for host
/// tests that exercise source routing without a live session.
pub use lash_core::testing::tool_registry_with_live_provider;
/// A held process for host tests: an engine input of [`HeldProcessEngine`]'s
/// kind, a registration of one under the fixture environment, and the plugin
/// factory that contributes the engine to a host's core. A held process runs
/// until it is cancelled.
pub use lash_core::testing::{
    HeldProcessEngine, held_engine_input, held_engine_registration, process_engine_plugin_fixture,
    process_execution_env_fixture,
};
pub use lash_core::testing::{
    MockSessionManager, TestClock, TestProvider, TestProviderBuilder, mock_attempt_context,
    mock_attempt_context_with_execution_binding, test_code_protocol_factories,
};
/// Model catalogs for host tests: a registry serving one model through a
/// test provider, and the metadata and recorded selection it mints.
pub use lash_core::testing::{
    single_llm_profile_registry, standard_test_llm_profiles, test_llm_profile_config,
    test_llm_profile_metadata,
};

/// [`RuntimeExecutionContext`](crate::tools::RuntimeExecutionContext)
/// constructors for host tests that shift context-bound execution —
/// e.g. [`tools::compile_with_deferred_resolution`](crate::tools::compile_with_deferred_resolution)
/// — without a production runtime.
///
/// Each takes the [`TestExecutionPorts`] it runs over: a backend's ports
/// (`&backend` converts), a backend's with the controller one engine
/// execution lent through [`TestExecutionPorts::lent`] (the context then
/// cannot outlive that execution), or a host's with
/// [`TestExecutionPorts::over_host`]. There is no in-memory default.
pub use lash_core::testing::{
    TestExecutionPorts, cancelled_code_execution_context, code_execution_context,
    code_execution_context_for_process, code_execution_context_with_invocation,
    code_execution_context_with_process_dependencies, code_execution_context_with_tool_catalog,
    code_execution_context_with_tool_provider_and_catalog,
    code_execution_context_with_tool_provider_catalog_and_invocation,
    code_execution_context_with_tool_provider_catalog_scoped_effect_controller_and_invocation,
    exec_code_invocation,
};

#[cfg(test)]
pub(crate) fn runtime_lease_owner() -> lash_core::LeaseOwnerIdentity {
    lash_core::LeaseOwnerIdentity::opaque(
        lash_core::LeaseOwnerId::new("lash-runtime-test-worker"),
        lash_core::LeaseIncarnationId::new("lash-runtime-test-boot"),
    )
}

/// The normalized behavior-transcript vocabulary. Render a scenario's real facts
/// into it and pin the result with an inline `insta` snapshot; see
/// `docs/adr/0050-behavior-transcripts-are-one-normalized-vocabulary.md`.
pub use lash_core::testing::behavior_transcript;

/// Store-factory decorator that observes accepted runtime-checkpoint commits, so
/// a scenario can render durable-write transcript lines from real facts.
pub use lash_core::testing::checkpoint_observer;

/// The decoded plugin-namespace map a
/// [`PluginState`](checkpoint_observer::CheckpointComponentWriteKind::PluginState)
/// checkpoint write carries: each namespace's values address and host-owned
/// generation (FIG-5301).
pub use lash_core_store::plugin_state::PluginStateMap;

/// Store-construction fixtures shared by kernel tests and certification
/// scenarios: session-store requests, lease claims, commit helpers, and the
/// completion-deferral authorization seam.
pub use lash_core::testing::store_fixtures;

/// The error side of a scripted store fault: each listed operation's error
/// type lifts an injected `StoreError`, so a fault reaches the caller as that
/// operation's own error (ADR 0044 §Simulation).
pub use lash_core::testing::ScriptedError;

/// Waits until a state no event reports — a fixture's atomic, a server's
/// admin view — holds, for laws that poll what the engine's own tasks reach.
pub use lash_core::testing::wait_until;

// The vocabulary this module's signatures name (the facade-completeness rule).

/// Make panic containment loud in a test or performance harness.
pub use lash_core::panic_containment::{is_loud, set_loud};
/// Turn-phase instrumentation for tests and performance harnesses. Phase names
/// follow the runtime implementation and are not a production host contract.
pub use lash_core::runtime::{
    RuntimeNamedPhase, RuntimeTurnPhase, RuntimeTurnPhaseProbe, RuntimeTurnPhaseProbeSlot,
};

/// What a session activation runs a session's turns with: machine build,
/// model calls, tool rounds, code cells and the head commit. Nameable for
/// [`session_turn_services`]' answer; its own surface is the runtime's, not
/// the facade's.
#[doc(hidden)]
pub use lash_core::runtime::durable::session::TurnServices;

/// The turn services a [`LashCore`](crate::LashCore)'s node runs its
/// sessions' turns with, for a test that runs the session actors on nodes of
/// its own (a core built with
/// [`serve_sessions(false)`](crate::core::LashCoreBuilder::serve_sessions)).
pub fn session_turn_services(core: &crate::LashCore) -> std::sync::Arc<dyn TurnServices> {
    core.turn_services()
}

/// The backend and activation a [`LashCore`](crate::LashCore)'s node serves,
/// each commit reported to `probe`: its sessions' turns and its processes,
/// the process actors on the core's process worker. For a test that runs the
/// core's actors on nodes of its own (a core built with
/// [`serve_sessions(false)`](crate::core::LashCoreBuilder::serve_sessions)),
/// so the nodes it kills run what the core's node runs.
///
/// # Errors
///
/// The core's process worker did not build.
pub fn node_activation(
    core: &crate::LashCore,
    probe: std::sync::Arc<dyn lash_core::durable_port::DurableProbe>,
) -> crate::Result<(
    crate::Backend,
    std::sync::Arc<dyn lash_core::durable_port::runner::Activation>,
)> {
    let activations = core.node_activations(probe);
    let process = activations.processes?;
    Ok((
        activations.backend,
        std::sync::Arc::new(lash_core::durable_port::ActorDispatch {
            session: activations.sessions,
            process,
        }),
    ))
}

/// Decode a command's recorded outcome while explicitly settling fixture setup.
/// These conversions are test support; hosts can decode their own settlement.
pub trait AdminFixtureOutcome: Sized {
    fn from_admin_outcome(outcome: crate::SessionCommandOutcome) -> crate::Result<Self>;
}

/// The decoder passed to [`crate::AdminMutation::settle_with`] by host fixtures.
pub fn admin_fixture_outcome<T: AdminFixtureOutcome>(
    outcome: crate::SessionCommandOutcome,
) -> crate::Result<T> {
    T::from_admin_outcome(outcome)
}

fn fixture_outcome_error(outcome: crate::SessionCommandOutcome) -> crate::EmbedError {
    match outcome {
        crate::SessionCommandOutcome::Failed { refusal } => {
            crate::EmbedError::Runtime(refusal.into())
        }
        crate::SessionCommandOutcome::PluginOperation {
            outcome: lash_core::runtime::PluginOperationCommandOutcome::Failed { failure },
        } => crate::EmbedError::Control(
            lash_core::facade_support::PluginOperationInvokeError::Failed(failure),
        ),
        crate::SessionCommandOutcome::PluginOperation {
            outcome: lash_core::runtime::PluginOperationCommandOutcome::Refused { refusal },
        } => crate::EmbedError::Runtime(refusal.into()),
        other => crate::EmbedError::Session(crate::SessionError::Protocol(format!(
            "unexpected fixture command outcome: {other:?}"
        ))),
    }
}

impl AdminFixtureOutcome for () {
    fn from_admin_outcome(outcome: crate::SessionCommandOutcome) -> crate::Result<Self> {
        match outcome {
            crate::SessionCommandOutcome::AppendSessionNodes {
                outcome: lash_core::AppendSessionNodesOutcome::Appended { .. },
            } => Ok(()),
            other => Err(fixture_outcome_error(other)),
        }
    }
}
impl AdminFixtureOutcome for bool {
    fn from_admin_outcome(outcome: crate::SessionCommandOutcome) -> crate::Result<Self> {
        match outcome {
            crate::SessionCommandOutcome::CompactContext {
                outcome: lash_core::runtime::CompactContextOutcome::Opened { .. },
            } => Ok(true),
            crate::SessionCommandOutcome::CompactContext {
                outcome: lash_core::runtime::CompactContextOutcome::NothingToCompact,
            } => Ok(false),
            crate::SessionCommandOutcome::CompactContext {
                outcome: lash_core::runtime::CompactContextOutcome::Failed { refusal },
            } => Err(crate::EmbedError::Runtime(refusal.into())),
            other => Err(fixture_outcome_error(other)),
        }
    }
}
impl AdminFixtureOutcome for lash_core::AppendSessionNodesOutcome {
    fn from_admin_outcome(outcome: crate::SessionCommandOutcome) -> crate::Result<Self> {
        match outcome {
            crate::SessionCommandOutcome::AppendSessionNodes { outcome } => Ok(outcome),
            other => Err(fixture_outcome_error(other)),
        }
    }
}
impl AdminFixtureOutcome for lash_core::OpenAgentFrameOutcome {
    fn from_admin_outcome(outcome: crate::SessionCommandOutcome) -> crate::Result<Self> {
        match outcome {
            crate::SessionCommandOutcome::OpenAgentFrame {
                outcome: lash_core::runtime::OpenAgentFrameCommandOutcome::Opened { outcome },
            } => Ok(outcome),
            crate::SessionCommandOutcome::OpenAgentFrame {
                outcome: lash_core::runtime::OpenAgentFrameCommandOutcome::Refused { refusal },
            } => Err(crate::EmbedError::Runtime(refusal.into())),
            other => Err(fixture_outcome_error(other)),
        }
    }
}
impl AdminFixtureOutcome for u64 {
    fn from_admin_outcome(outcome: crate::SessionCommandOutcome) -> crate::Result<Self> {
        match outcome {
            crate::SessionCommandOutcome::ToolState {
                outcome: lash_core::facade_support::ToolStateChangeOutcome::Applied { generation },
            } => Ok(generation),
            crate::SessionCommandOutcome::ToolState {
                outcome: lash_core::facade_support::ToolStateChangeOutcome::Refused { error },
            } => Err(error.into()),
            other => Err(fixture_outcome_error(other)),
        }
    }
}
impl AdminFixtureOutcome for lash_core::ToolRestoreReport {
    fn from_admin_outcome(outcome: crate::SessionCommandOutcome) -> crate::Result<Self> {
        match outcome {
            crate::SessionCommandOutcome::ToolState {
                outcome: lash_core::facade_support::ToolStateChangeOutcome::Restored { report },
            } => Ok(report),
            crate::SessionCommandOutcome::ToolState {
                outcome: lash_core::facade_support::ToolStateChangeOutcome::Refused { error },
            } => Err(error.into()),
            other => Err(fixture_outcome_error(other)),
        }
    }
}
impl<T: serde::de::DeserializeOwned> AdminFixtureOutcome
    for lash_core::facade_support::PluginOperationReceipt<T>
{
    fn from_admin_outcome(outcome: crate::SessionCommandOutcome) -> crate::Result<Self> {
        match outcome {
            crate::SessionCommandOutcome::PluginOperation {
                outcome:
                    lash_core::runtime::PluginOperationCommandOutcome::Completed {
                        plugin_id,
                        output,
                        events,
                        pending_turn_inputs,
                    },
            } => Ok(Self {
                output: serde_json::from_value(output).map_err(|error| {
                    crate::EmbedError::Session(crate::SessionError::Protocol(error.to_string()))
                })?,
                events: events
                    .into_iter()
                    .map(|value| lash_core::facade_support::PluginOwned {
                        plugin_id: plugin_id.clone(),
                        value,
                    })
                    .collect(),
                pending_turn_inputs,
            }),
            other => Err(fixture_outcome_error(other)),
        }
    }
}

/// A core's language-observation dispatcher over a process replay store of
/// the test's own: the bounded ingress and the worker a
/// [`LashCore`](crate::LashCore) installs as its observation sink, for a law
/// that measures what the dispatcher does to a store under load.
pub struct LanguageObservationDispatcher {
    publisher: std::sync::Arc<crate::language_observation::LanguageObservationPublisher>,
}

impl LanguageObservationDispatcher {
    /// The dispatcher a core builds over `process_store`. Call it inside the
    /// runtime its worker runs on.
    pub fn over(process_store: std::sync::Arc<dyn lash_core::ProcessReplayStore>) -> Self {
        Self {
            publisher: std::sync::Arc::new(
                crate::language_observation::LanguageObservationPublisher::new(
                    process_store,
                    std::sync::Arc::new(lash_core::facade_support::InMemoryLiveReplayStore::new(
                        lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
                    )),
                ),
            ),
        }
    }

    /// Admit one language observation the way the VM's trace does: a
    /// synchronous call that never waits on the store.
    pub fn observe(&self, observation: lash_core::LanguageExecutionObservation) {
        let record = lash_trace::TraceRecord {
            schema_version: lash_trace::TRACE_SCHEMA_VERSION,
            id: observation.execution.event_key.clone(),
            content: lash_trace::TelemetryContent::Captured,
            timestamp: (std::time::UNIX_EPOCH
                + std::time::Duration::from_millis(observation.observed_at_ms))
            .into(),
            context: lash_trace::TraceContext::default(),
            event: lash_trace::TraceEvent::LanguageExecution {
                language: observation.language,
                event: observation.execution,
            },
        };
        // The dispatcher's sink never refuses a record.
        let _ = lash_trace::TraceSink::append(self.publisher.as_ref(), &record);
    }

    /// Stop the dispatcher; an undrained observation is explicit loss.
    pub async fn shutdown(&self) {
        self.publisher.shutdown().await;
    }
}
