//! EVENT-ONCE: an MCP event subscription killed at every commit label
//! reacts to each event exactly once (FIG-6178).
//!
//! A host subscribes a session to a SEP-3415 fixture server in poll mode.
//! The server holds two events; it delivers both, then, asked from the
//! saved cursor, delivers both again (a redelivery within its retention).
//! Each event reacts as a keyed turn of the session, run by a scripted
//! model. The nodes are simulated (A and B) over the production SQLite
//! store; the fixture server is one process that outlives them, and the
//! virtual clock waits out every fetch in flight.
//!
//! The matrix cuts the uncut run at every labelled write (the poll's
//! commit, the commit that saves the cursor with the dispatch's admission,
//! the dispatch's outcome, the turns') under fail-before, ack-hidden,
//! zombie, abort and commit-then-abort, recovers on the other node, and
//! checks:
//!
//! - each event's turn committed exactly once, and the session holds one
//!   reaction per event: neither a crash nor the redelivery starts another;
//! - a zombie's writes after its reap are refused.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/sim.rs"]
mod sim;

#[path = "support/matrix.rs"]
mod matrix;

use matrix::MatrixTestExt as _;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash::mcp::McpPluginFactory;
use lash::mcp::events::testing::{FIXTURE_EVENT, McpEventsFixture};
use lash::mcp::events::{
    McpDeliveryMode, McpEventsActivity, McpEventsConfig, McpReactionTarget, McpSubscribeRequest,
};
use lash_core::LlmOutputPart;
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{LlmRequest, LlmResponse, LlmStreamEvent, StreamBlockIdentity};
use lash_core_execution::{Backend, BackendParts, NoProjectionProviders, ProcessId, StoreSet};
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableError, DurableStore};
use lash_durable_test::{
    Cut, Fault, Matrix, OffClockWork, Scenario, SimClock, SimNodes, SimNodesConfig, Stored,
    Tripwire, WriteKind,
};
use lash_sansio::SessionId;
use lash_sansio::llm::types::{StreamBlockEvent, StreamBlockKind};
use lash_sansio::sync::MutexExt as _;

const SESSION: &str = "mcp-events-subscriber";
const MODEL: &str = "mcp-events-model";
const SERVER: &str = "fx";
const EVENTS: [&str; 2] = ["e1", "e2"];
/// Virtual time a run polls for at least: the first poll, the redelivery
/// a second later, and one more.
const SETTLED_MS: u64 = 2_500;

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

fn process_actor(process: &ProcessId) -> ActorKey {
    ActorKey::process(process.as_str()).expect("a process actor key")
}

fn session_actor() -> ActorKey {
    ActorKey::session(SESSION).expect("a session actor key")
}

/// The scripted model: it notes every event.
fn model() -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("mcp-events-scripted")
        .requires_streaming(true)
        .complete(|request: LlmRequest| async move {
            if let Some(stream) = request.stream_events.as_ref() {
                stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                    kind: StreamBlockKind::AssistantText,
                    block: StreamBlockIdentity::new("text:0", 0),
                    text: "noted".to_owned(),
                }));
            }
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "noted".to_owned(),
                    response_meta: None,
                }],
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle()
}

fn metadata() -> lash_core::LlmProfileMetadata {
    lash_core::LlmProfileMetadata::builder(MODEL)
        .cache_retention(lash_core::provider::CacheRetention::Short)
        .context_window_tokens(200_000)
        .build()
        .expect("the model's metadata")
}

fn spec() -> lash::SessionSpec {
    lash::SessionSpec::new(
        MODEL,
        lash::TurnBudget::Unbounded,
        lash::MaxToolCalls::new(8),
    )
    .no_progress_budget(lash_core::NoProgressBudget::bounded(12))
}

/// The fetches in flight, which the virtual clock waits out: a poll runs
/// off the clock, on the fixture server's process.
struct Fetches(Arc<McpEventsActivity>);

impl OffClockWork for Fetches {
    fn busy(&self) -> bool {
        self.0.in_flight() > 0
    }

    fn ended(&self) -> usize {
        self.0.ended()
    }
}

struct EventOnce {
    tripwire: Arc<Tripwire>,
    fixture: McpEventsFixture,
    _world: tempfile::TempDir,
    backend: Mutex<Option<Backend>>,
    plugin: Mutex<Option<Arc<McpPluginFactory>>>,
    core: Mutex<Option<lash::LashCore>>,
    subscription: Mutex<Option<ProcessId>>,
}

impl EventOnce {
    fn new() -> Self {
        let world = tempfile::tempdir().expect("the fixture's world");
        let fixture = McpEventsFixture::new(world.path());
        fixture.control(serde_json::json!({ "null_cursor": "origin", "next_poll_ms": 1000 }));
        for event in EVENTS {
            fixture.emit(event, serde_json::json!({ "ticket": event }));
        }
        Self {
            tripwire: Arc::default(),
            fixture,
            _world: world,
            backend: Mutex::default(),
            plugin: Mutex::default(),
            core: Mutex::default(),
            subscription: Mutex::default(),
        }
    }

    fn backend(&self) -> Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }

    fn plugin(&self) -> Arc<McpPluginFactory> {
        self.plugin
            .lock_recover()
            .clone()
            .expect("the plugin is built with the database")
    }

    /// The deployment's core over the scenario's backend: it serves no node
    /// of its own, the simulated nodes run its actors.
    fn core(&self) -> lash::LashCore {
        let backend = self.backend();
        let plugin = self.plugin();
        self.core
            .lock_recover()
            .get_or_insert_with(|| {
                let core = lash::LashCore::standard_builder(backend)
                    .plugin(Arc::clone(&plugin) as Arc<dyn lash::plugins::PluginFactory>)
                    .serve_sessions(false)
                    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
                    .data_retention(lash::DataRetention::standard())
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .execution_budgets(lash::ExecutionBudgets::recommended())
                    .delta_coalescing(lash::DeltaCoalescing::recommended())
                    .serve_test_llm_profile(model(), metadata())
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(
                        lash::persistence::LeaseOwnerId::new("mcp-events-deployment"),
                        lash::persistence::LeaseIncarnationId::new("mcp-events-boot"),
                    ))
                    .expect("the core builds");
                core.bind_mcp_events(&plugin.events());
                core
            })
            .clone()
    }

    fn subscription(&self) -> Option<ProcessId> {
        self.subscription.lock_recover().clone()
    }

    /// How many of the session's turns reacted to `event`.
    async fn reactions(&self, event: &str) -> Result<usize, String> {
        let durable = self
            .core()
            .session(session())
            .durable()
            .await
            .map_err(|error| format!("the durable session: {error}"))?;
        let Some(view) = durable
            .read()
            .await
            .map_err(|error| format!("the session's committed read: {error}"))?
        else {
            return Ok(0);
        };
        let marker = format!("\"eventId\": \"{event}\"");
        Ok(view
            .messages()
            .iter()
            .flat_map(|message| message.parts.iter())
            .map(|part| part.content().matches(marker.as_str()).count())
            .sum())
    }

    /// Deliveries the subscription made or found made, as of its last
    /// transition.
    async fn delivered(&self) -> u64 {
        let Some(subscription) = self.subscription() else {
            return 0;
        };
        match self.plugin().events().status(&subscription).await {
            Ok(Some(status)) => status.delivered,
            _ => 0,
        }
    }
}

#[async_trait::async_trait]
impl Scenario for EventOnce {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let stores = sim::memory(Arc::clone(&clock)).await;
        let database: Arc<dyn DurableStore> = Arc::new(stores.durable_store());
        let stores: Arc<dyn StoreSet> = Arc::new(stores);
        let backend = Backend::assemble(BackendParts {
            stores,
            settings: sim::settings(),
            engines: Vec::new(),
            providers: Arc::new(NoProjectionProviders),
            formats: lash::formats::actor_state_surfaces(),
        })
        .expect("the backend assembles");
        *self.backend.lock_recover() = Some(backend);
        let plugin = Arc::new(
            McpPluginFactory::new(BTreeMap::from([(
                SERVER.to_owned(),
                self.fixture
                    .server(McpEventsConfig::new([McpDeliveryMode::Poll])),
            )]))
            .await
            .expect("the MCP plugin builds"),
        );
        clock.wait_out(Arc::new(Fetches(plugin.events().activity())));
        *self.plugin.lock_recover() = Some(plugin);
        database
    }

    fn config(&self) -> SimNodesConfig {
        let (backend, _) =
            lash::testing::node_activation(&self.core(), Arc::clone(&self.tripwire) as _)
                .expect("the core's node activation");
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: backend.formats().decodes(),
            serves: lash::testing::node_serves(&self.core()),
            max_active: 4,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        lash::testing::node_activation(&self.core(), Arc::clone(&self.tripwire) as _)
            .expect("the core's node activation")
            .1
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        let core = self.core();
        core.session(session())
            .create(lash::SessionCreation::root(spec()))
            .await
            .map_err(|error| format!("create the session: {error}"))?;
        let subscription = self
            .plugin()
            .events()
            .subscribe(McpSubscribeRequest::host(
                SERVER,
                FIXTURE_EVENT,
                serde_json::Map::new(),
                McpReactionTarget::Session { session: session() },
                "event-once",
            ))
            .await
            .map_err(|error| format!("subscribe: {error}"))?;
        *self.subscription.lock_recover() = Some(subscription);
        // The server delivers both events again once asked from the saved
        // cursor.
        self.fixture.control(serde_json::json!({
            "null_cursor": "origin",
            "next_poll_ms": 1000,
            "rewind_once": EVENTS.len(),
        }));
        nodes.start("a");
        nodes.quiesce().await;
        nodes.start("b");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        let mut actors = vec![session_actor()];
        actors.extend(self.subscription().as_ref().map(process_actor));
        actors
    }

    /// Both events reacted, the session is at rest, and the subscription
    /// polled on for a while: long enough for the redelivery, which a cut
    /// that loses the poll that made it never commits.
    async fn done(&self, nodes: &SimNodes) -> bool {
        if nodes.clock().logical_ms() < SETTLED_MS {
            return false;
        }
        for event in EVENTS {
            if !matches!(self.reactions(event).await, Ok(count) if count > 0) {
                return false;
            }
        }
        matches!(
            nodes.database().actor(&session_actor()).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Idle
        )
    }

    async fn check(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let trace = nodes.script().trace();
        let turns = trace
            .iter()
            .filter(|write| write.point.label == CommitLabel::TURN_COMMIT && write.committed())
            .count();
        if turns != EVENTS.len() {
            violations.push(format!(
                "{turns} reaction turns committed for {} events",
                EVENTS.len()
            ));
        }
        for event in EVENTS {
            match self.reactions(event).await {
                Ok(1) => {}
                other => violations.push(format!("event {event} reacted {other:?} times")),
            }
        }
        match cut {
            Some(cut) => violations.extend(zombie_laws(cut, &trace)),
            // The uncut run saw the server redeliver both events.
            None => {
                let delivered = self.delivered().await;
                if delivered < 2 * EVENTS.len() as u64 {
                    violations.push(format!(
                        "the uncut run delivered {delivered} times, not every event twice"
                    ));
                }
            }
        }
        violations
    }
}

/// Once a zombie's actors moved, every owner write it attempts is refused
/// with `OwnershipLost`.
fn zombie_laws(cut: &Cut, trace: &[lash_durable_test::Write]) -> Vec<String> {
    let mut violations = Vec::new();
    if cut.fault != Fault::Zombie || cut.kind != WriteKind::Actor {
        return violations;
    }
    let Some(at) = trace.iter().position(|write| {
        write.node == cut.node && write.point == cut.point && write.cut == Some(cut.fault)
    }) else {
        return vec!["the zombie's cut write is not in the trace".to_owned()];
    };
    for write in trace[at..]
        .iter()
        .filter(|write| write.node == cut.node && write.kind == WriteKind::Actor)
    {
        match &write.stored {
            Stored::Refused(DurableError::OwnershipLost(_)) => {}
            other => violations.push(format!("zombie write {write} was {other:?}")),
        }
    }
    violations
}

/// A subscription killed at every label reacts to each event once.
#[tokio::test]
async fn an_event_subscription_killed_at_every_label_reacts_to_each_event_once() {
    let report = Matrix::new()
        .faults(&[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ])
        .across_nodes()
        .horizon(Duration::from_secs(600))
        .run_test(EventOnce::new)
        .await;
    let labels: Vec<&str> = report.labels().iter().map(|label| label.as_str()).collect();
    eprintln!(
        "mcp events: {} cells over {} labels ({})",
        report.cells.len(),
        labels.len(),
        labels.join(", ")
    );
    // The uncut run's owner commits, in order: what each event costs.
    let owner: Vec<&str> = report
        .baseline
        .iter()
        .filter(|write| write.kind == WriteKind::Actor && write.committed())
        .map(|write| write.point.label.as_str())
        .collect();
    eprintln!(
        "mcp events: uncut owner commits ({}): {}; {:?}",
        owner.len(),
        owner.join(", "),
        report.elapsed
    );
    report.assert_held();
    for label in [
        CommitLabel::PROCESS_ADVANCE,
        CommitLabel::STEP_OUTCOME,
        CommitLabel::TURN_COMMIT,
    ] {
        assert!(
            report.labels().contains(&label),
            "the matrix never cut {label}"
        );
    }
}
