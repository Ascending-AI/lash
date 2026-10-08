//! A durable turn's trace scope across its resumes (FIG-5363), through a
//! host's `send()` on a served node.
//!
//! The turn's admission retains its `DurableTraceScope`: written once, by
//! the admission, and read back unchanged by every later admission of the
//! turn. The phase checkpoint carries it, so a turn resumed from its rows
//! reads it back instead of proposing a new one.
//!
//! Each law serves the core with a recording telemetry adapter
//! (`support/telemetry.rs`). A turn parks on a deferred call and its
//! session releases `waiting`; the call is resolved out of band and the
//! turn resumes from its checkpoint, on the node that parked it or on a
//! node that took the session over.
//!
//! - **One admission:** the turn's scope is selected exactly once, the
//!   `lash.turn.admitted` span an adapter exports.
//! - **One trace:** every record of the turn carries the trace its
//!   admission started, and every record of the turn's own scope its
//!   admission's anchor.
//! - **One tool admission (FIG-5382):** the parked call's scope is selected
//!   exactly once, the `lash.tool.admitted` span an adapter exports, on the
//!   turn's trace: the resume reads the scope its round's admission retained
//!   back instead of admitting the call again. A taking-over node is
//!   another process, whose adapter remembers nothing the parking node
//!   exported: the call's admission export is owed only until an owner
//!   discharged it, and the takeover reconciles nothing (FIG-5452).
//! - **Admissions across a lost node (FIG-5457):** a node that loses its
//!   life with an admission it exported but no phase or outcome after it is
//!   taken over by another process, whose adapter remembers nothing the
//!   lost node exported. A turn whose node is lost between `turn.admit` and
//!   `model.start`, and a code cell's call whose node is lost while its
//!   body runs, are each admitted exactly once: the lost node discharged
//!   the admission's export durably, so the takeover reconciles nothing.
//!
//! The crash half, a turn cut at a phase commit and resumed on the other
//! node, is `Turn::Trace` in `tool_crash_laws.rs`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;
#[path = "support/telemetry.rs"]
mod telemetry;

use std::sync::Arc;
use std::time::Duration;

use lash_core::ToolCall;
use lash_core::ToolDefinitionBindingExt as _;

use served::{Tier, WATCHDOG, World};
use telemetry::Telemetry;

/// The tool that parks on its completion key, which the law resolves.
const DEFERRED: &str = "trace_deferred";

fn definition() -> lash_core::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_core::ToolDefinition::raw(
        format!("tool:{DEFERRED}"),
        DEFERRED,
        "Parks until the trace law resolves it.",
        object.clone(),
        object,
    )
    .expect("the tool's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], DEFERRED))
    .with_declaration(lash_core::ToolDeclaration::deferring())
    .with_park(lash_core::ParkBound::Within(
        std::time::Duration::from_secs(120),
    ))
}

/// The law's tool: it parks, and hands the law its completion key.
struct DeferredTool {
    keys: tokio::sync::mpsc::UnboundedSender<String>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for DeferredTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == DEFERRED).then(|| Arc::new(definition().contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let key = call
            .context
            .completion_key()
            .expect("a deferring call's round pins its completion wait");
        self.keys
            .send(key.as_str().to_owned())
            .expect("the law takes the key");
        lash_core::ToolAttemptOutcome::Pending(lash_core::PendingCompletion::new())
    }
}

/// The builder of a core serving the law's tool and telemetry.
fn builder(
    telemetry: &Telemetry,
    keys: &tokio::sync::mpsc::UnboundedSender<String>,
) -> impl FnOnce(&lash::Backend) -> lash::LashCoreBuilder {
    let telemetry = telemetry.clone();
    let keys = keys.clone();
    move |backend| {
        lash::LashCore::standard_builder(backend.clone())
            .tools(Arc::new(DeferredTool { keys }))
            .trace_runtime(telemetry.runtime())
    }
}

/// Wait until `session`'s actor is released `waiting`.
async fn released_waiting(backend: &lash::Backend, session: &str) {
    let actor = lash_durable::ActorKey::session(session).expect("a session actor key");
    tokio::time::timeout(WATCHDOG, async {
        loop {
            let snapshot = backend
                .durable()
                .actor(&actor)
                .await
                .expect("the actor is read");
            if snapshot.is_some_and(|snapshot| snapshot.state == lash_durable::ActorState::Waiting)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("deadlock watchdog: the session never released `waiting`");
}

/// Where the parked turn resumes.
#[derive(Clone, Copy, Debug)]
enum Resume {
    /// On the node that parked it.
    SameNode,
    /// On a node that took the session over.
    Takeover,
}

/// A turn admitted, parked on its deferred call and resumed per `resume`
/// is admitted once, and its records carry one trace.
async fn resumed_turn_keeps_its_trace_scope(tier: Tier, resume: Resume) {
    let telemetry = Telemetry::default();
    let (keys, mut parked) = tokio::sync::mpsc::unbounded_channel();
    // The owner keeps the parked session hot only briefly, so the
    // resolution wakes a released actor that a claim resumes.
    let settings = lash_core_execution::DurableSettings {
        idle_evict: Duration::from_millis(50),
        ..lash_core_execution::DurableSettings::default()
    };
    let Some(mut world) = World::configured(tier, settings, builder(&telemetry, &keys)).await
    else {
        return;
    };
    let name = format!("trace-resume-{resume:?}").to_lowercase();
    world.script(
        &name,
        vec![served::response(vec![served::call(
            "call-1",
            DEFERRED,
            serde_json::json!({}),
        )])],
    );
    let session = world.session(&name, served::spec(8)).await;
    let sent = session
        .send(lash::TurnInput::text(name.as_str()))
        .await
        .expect("the input is accepted");
    let key = tokio::time::timeout(WATCHDOG, parked.recv())
        .await
        .expect("deadlock watchdog: the deferred call never ran")
        .expect("the tool hands over its key");
    released_waiting(&world.backend, &name).await;
    if let Resume::Takeover = resume {
        // The taking-over node is another process: its adapter remembers
        // nothing the parking node exported (FIG-5452).
        world
            .restart(
                "trace-takeover-boot",
                builder(&telemetry.restarted(), &keys),
            )
            .await;
    }
    lash_core::waits::resolve_host(
        &world.backend,
        &key,
        lash_core::Resolution::Ok(serde_json::json!({ "answered": true })),
    )
    .await
    .expect("the deferred call's wait resolves");
    match resume {
        Resume::SameNode => {
            let output = tokio::time::timeout(WATCHDOG, sent.output())
                .await
                .expect("deadlock watchdog: the resumed turn never settled")
                .expect("the turn answers");
            served::assert_answered("the resumed turn", &output);
        }
        Resume::Takeover => {
            // The send's handle belonged to the stopped node: a follow-up
            // on the taking-over node settles once the resumed turn has.
            drop(sent);
            let session = world
                .core
                .session(lash::SessionId::try_from(name.clone()).expect("a session id"))
                .durable()
                .await
                .expect("the session is open on the taking-over node");
            let output = world.send(&session, "after the takeover").await;
            served::assert_answered("the follow-up turn", &output);
        }
    }
    let mut violations = telemetry.first_turn_violations(&name);
    violations.extend(telemetry.first_turn_tool_violations(&name));
    world.shutdown().await;
    assert!(
        violations.is_empty(),
        "{resume:?}: the resumed turn broke its trace scope:\n  {}",
        violations.join("\n  ")
    );
}

async fn a_turn_resumed_on_its_node_keeps_its_trace_scope(tier: Tier) {
    resumed_turn_keeps_its_trace_scope(tier, Resume::SameNode).await;
}

async fn a_turn_resumed_on_a_takeover_node_keeps_its_trace_scope(tier: Tier) {
    resumed_turn_keeps_its_trace_scope(tier, Resume::Takeover).await;
}

/// The tool a code cell's call runs, which holds on the node the law
/// loses.
const HELD: &str = "trace_held";

/// The plugin whose before-turn callback holds on the node the law loses.
const HOLD_PLUGIN: &str = "trace-hold-law";

fn held_definition() -> lash_core::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_core::ToolDefinition::raw(
        format!("tool:{HELD}"),
        HELD,
        "Holds on the node the trace law loses.",
        object.clone(),
        object,
    )
    .expect("the tool's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], HELD))
    // A call its node lost while it ran runs again on the node that takes
    // the cell over, which builds its body without the candidate.
    .with_execution_policy(lash_core::ExecutionPolicy::repeatable(
        std::num::NonZeroU32::new(3).expect("a nonzero attempt bound"),
        1,
        1,
    ))
}

/// What holds on the node the law loses: told on `held` once it holds,
/// and never answered. `None` on the node that takes over, where nothing
/// holds.
#[derive(Clone)]
struct Hold {
    held: Option<tokio::sync::mpsc::UnboundedSender<()>>,
}

impl Hold {
    /// Hold forever on the lost node; go on at once on any other.
    async fn hold(&self) {
        if let Some(held) = &self.held {
            held.send(()).expect("the law waits for the hold");
            std::future::pending::<()>().await;
        }
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Hold {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![held_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == HELD).then(|| Arc::new(held_definition().contract()))
    }

    async fn execute(&self, _call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.hold().await;
        lash_core::ToolOutcome::ok(serde_json::json!({ "answered": true })).into()
    }
}

impl lash::plugins::PluginDefinition for Hold {
    fn declaration() -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial(HOLD_PLUGIN)
    }
}

impl lash::plugins::PluginFactory for Hold {
    fn id(&self) -> &'static str {
        HOLD_PLUGIN
    }

    fn build(
        &self,
        _: &lash::plugins::PluginSessionContext,
    ) -> Result<Arc<dyn lash::plugins::SessionPlugin>, lash::plugins::PluginError> {
        Ok(Arc::new(self.clone()))
    }
}

impl lash::plugins::SessionPlugin for Hold {
    fn id(&self) -> &'static str {
        HOLD_PLUGIN
    }

    fn register(
        &self,
        reg: &mut lash::plugins::PluginRegistrar,
    ) -> Result<(), lash::plugins::PluginError> {
        let hold = self.clone();
        reg.turn().before(
            lash::hook_key!("hold"),
            Arc::new(move |_| {
                let hold = hold.clone();
                Box::pin(async move {
                    hold.hold().await;
                    Ok(lash_core::plugin::TurnContributions::default())
                })
            }),
        )
    }
}

/// Where the lost node holds.
#[derive(Clone, Copy, Debug)]
enum Lost {
    /// In the turn's before-turn callback: after `turn.admit` committed and
    /// its candidate was selected, before `model.start`.
    BeforeModelStart,
    /// In the body of the call a code cell's quiet point admitted, once its
    /// candidate was selected.
    InCellCall,
}

/// The builder of a core serving `telemetry` and `hold`: an RLM core whose
/// cells call the held tool, or a standard core whose turns run the
/// holding before-turn callback.
fn held_builder(
    lost: Lost,
    telemetry: &Telemetry,
    hold: Hold,
) -> impl FnOnce(&lash::Backend) -> lash::LashCoreBuilder {
    let telemetry = telemetry.clone();
    move |backend| {
        let builder = match lost {
            Lost::BeforeModelStart => {
                lash::LashCore::standard_builder(backend.clone()).plugin(Arc::new(hold))
            }
            Lost::InCellCall => lash::LashCore::rlm_builder(
                backend.clone(),
                served::rlm(backend, None, sim::untimed_workers()),
            )
            .tools(Arc::new(hold)),
        };
        builder.trace_runtime(telemetry.runtime())
    }
}

/// A turn whose node is lost where `lost` says, once the admission there
/// was exported, and which a node of another process takes over, is
/// admitted once, as is its cell's call (FIG-5457).
async fn a_lost_nodes_admission_is_exported_once(tier: Tier, lost: Lost) {
    let telemetry = Telemetry::default();
    let (held, mut holding) = tokio::sync::mpsc::unbounded_channel();
    let Some(mut world) = World::new(
        tier,
        held_builder(lost, &telemetry, Hold { held: Some(held) }),
    )
    .await
    else {
        return;
    };
    let name = format!("trace-lost-{lost:?}").to_lowercase();
    if let Lost::InCellCall = lost {
        world.script(
            &name,
            vec![served::cell(&format!("await tools.{HELD}({{}});"))],
        );
    }
    let session = world.session(&name, served::spec(8)).await;
    let sent = session
        .send(lash::TurnInput::text(name.as_str()))
        .await
        .expect("the input is accepted");
    tokio::time::timeout(WATCHDOG, holding.recv())
        .await
        .expect("deadlock watchdog: the lost node never held")
        .expect("the hold tells the law");
    // The node is lost where it holds, and a node of another process takes
    // the session over: its adapter remembers nothing the lost node
    // exported.
    world
        .restart(
            "trace-lost-takeover-boot",
            held_builder(lost, &telemetry.restarted(), Hold { held: None }),
        )
        .await;
    // The send's handle belonged to the lost node: a follow-up on the
    // taking-over node settles once the taken-over turn has.
    drop(sent);
    let session = world
        .core
        .session(lash::SessionId::try_from(name.clone()).expect("a session id"))
        .durable()
        .await
        .expect("the session is open on the taking-over node");
    let output = world.send(&session, "after the takeover").await;
    served::assert_answered("the follow-up turn", &output);
    let mut violations = telemetry.first_turn_violations(&name);
    if let Lost::InCellCall = lost {
        violations.extend(telemetry.first_turn_tool_violations(&name));
    }
    world.shutdown().await;
    assert!(
        violations.is_empty(),
        "{lost:?}: the taken-over turn broke its admissions:\n  {}",
        violations.join("\n  ")
    );
}

async fn a_turn_whose_node_is_lost_before_its_model_starts_is_admitted_once(tier: Tier) {
    a_lost_nodes_admission_is_exported_once(tier, Lost::BeforeModelStart).await;
}

async fn a_cells_call_whose_node_is_lost_while_it_runs_is_admitted_once(tier: Tier) {
    a_lost_nodes_admission_is_exported_once(tier, Lost::InCellCall).await;
}

tiered_laws!(
    a_turn_resumed_on_its_node_keeps_its_trace_scope,
    a_turn_resumed_on_a_takeover_node_keeps_its_trace_scope,
    a_turn_whose_node_is_lost_before_its_model_starts_is_admitted_once,
    a_cells_call_whose_node_is_lost_while_it_runs_is_admitted_once,
);
