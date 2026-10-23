//! A durable turn's trace scope across its resumes (FIG-5363), through a
//! host's `send()` on a served node.
//!
//! The turn's admission retains its `DurableTraceScope`: written once, by
//! the admission, and read back unchanged by every later admission of the
//! turn. The phase checkpoint carries it, so a turn resumed from its rows
//! reads it back instead of proposing a new one.
//!
//! Each law serves the core with a recording trace sink
//! (`support/telemetry.rs`) that every process publishes to. A turn parks
//! on a deferred call and its session releases `waiting`; the call is
//! resolved out of band and the turn resumes from its checkpoint, on the
//! node that parked it or on a node that took the session over.
//!
//! - **One close:** the turn's span closes once, on the owner whose
//!   acknowledged commit wrote its terminal (P1), and its opening and
//!   closing records name one span.
//! - **One trace:** every record of the turn carries a position on the
//!   trace of the send that caused it.
//! - **One tool close (FIG-5382):** the parked call's span closes once, on
//!   the turn's trace: the resume reads the scope its round's admission
//!   retained back instead of admitting the call again, and an owner that
//!   did not write the call's outcome publishes no close for it.
//! - **Across a lost node (FIG-5457):** a node that loses its life after
//!   an admission, with no phase or outcome after it, is taken over by
//!   another process. A turn whose node is lost between `turn.admit` and
//!   `model.start`, and a code cell's call whose node is lost while its
//!   body runs, each close once, on the taking-over owner.
//! - **Unique ids:** no span id is published by two closing or whole
//!   records (L1(b)).
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

#[path = "support/packages.rs"]
mod packages;

use std::sync::Arc;
use std::time::Duration;

use lash_core::ToolCall;

use served::{Tier, WATCHDOG, World};
use telemetry::Telemetry;

/// The tool that parks on its completion key, which the law resolves.
const DEFERRED: &str = "trace_deferred";
const DEFERRED_OPERATION: &str = "tools.trace_deferred";

fn definition() -> lash_vocabulary::tool_contract::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_vocabulary::tool_contract::ToolDefinition::raw(
        format!("tool:{DEFERRED}"),
        DEFERRED_OPERATION,
        "Parks until the trace law resolves it.",
        object.clone(),
        object,
    )
    .expect("the tool's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_declaration(
        lash_vocabulary::tool_declaration::ToolDeclaration::deferring(),
        Some(lash_vocabulary::tool_contract::ParkBound::Within(
            std::time::Duration::from_secs(120),
        )),
    )
    .expect("a deferring tool declares its park bound")
}

/// The law's tool: it parks, and hands the law its completion key.
struct DeferredTool {
    keys: tokio::sync::mpsc::UnboundedSender<String>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for DeferredTool {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(vec![definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        (name == DEFERRED_OPERATION).then(|| Arc::new(definition().contract()))
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
            .plugin(packages::tools(
                lash::tool_provider_key!("tools"),
                Arc::new(DeferredTool { keys }),
            ))
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
/// closes once, and its records carry one trace.
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
            DEFERRED_OPERATION,
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
        // The taking-over node is another process, publishing to the same
        // collector.
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
                .session(
                    lash_vocabulary::identity::SessionId::try_from(name.clone())
                        .expect("a session id"),
                )
                .durable()
                .await
                .expect("the session is open on the taking-over node");
            let output = world.send(&session, "after the takeover").await;
            served::assert_answered("the follow-up turn", &output);
        }
    }
    let mut violations = telemetry.first_turn_violations(&name, false);
    violations.extend(telemetry.publication_violations(&[]));
    violations.extend(telemetry.first_turn_tool_violations(&name, false));
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

/// The builder of an RLM core whose cells call the law's deferred tool,
/// serving `telemetry`.
fn cell_builder(
    telemetry: &Telemetry,
    keys: &tokio::sync::mpsc::UnboundedSender<String>,
) -> impl FnOnce(&lash::Backend) -> lash::LashCoreBuilder {
    let telemetry = telemetry.clone();
    let keys = keys.clone();
    move |backend| {
        lash::LashCore::rlm_builder(
            backend.clone(),
            served::rlm(backend, sim::untimed_workers()),
        )
        .plugin(packages::tools(
            lash::tool_provider_key!("tools"),
            Arc::new(DeferredTool { keys }),
        ))
        .trace_runtime(
            telemetry
                .runtime()
                .with_content(lash::tracing::ContentPolicy::captured()),
        )
    }
}

/// A code cell that suspends on its parked call and is answered on a node
/// that took the session over is one span (the tracing lane T5, §4.5): the
/// taking-over owner, whose commit consumes the cell's answer, closes it
/// once, from the first start the suspended owner's phase retained, and the
/// cell's call is the cell span's child.
async fn suspended_cell_closes_on_takeover(tier: Tier, discard: bool) {
    let telemetry = Telemetry::default();
    let (keys, mut parked) = tokio::sync::mpsc::unbounded_channel();
    let settings = lash_core_execution::DurableSettings {
        idle_evict: Duration::from_millis(50),
        ..lash_core_execution::DurableSettings::default()
    };
    let Some(mut world) = World::configured(tier, settings, cell_builder(&telemetry, &keys)).await
    else {
        return;
    };
    let name = "trace-suspended-cell";
    world.script(name, vec![served::cell("await tools.traceDeferred({});")]);
    let session = world.session(name, served::spec(8)).await;
    let sent = session
        .send(lash::TurnInput::text(name))
        .id("trace-suspended-cell-run".into())
        .await
        .expect("the input is accepted");
    let key = tokio::time::timeout(WATCHDOG, parked.recv())
        .await
        .expect("deadlock watchdog: the cell's deferred call never ran")
        .expect("the tool hands over its key");
    released_waiting(&world.backend, name).await;
    let suspended = telemetry.first_turn_cell_closes(name);
    // The taking-over node is another process, publishing to the same
    // collector; everything it starts starts after this instant.
    let taken_over = lash::tracing::TraceTimestamp::from_system_time(std::time::SystemTime::now())
        .expect("a time after the epoch");
    world
        .restart(
            "trace-suspended-cell-boot",
            cell_builder(&telemetry.restarted(), &keys),
        )
        .await;
    if discard {
        world
            .core
            .session(lash_vocabulary::identity::SessionId::try_from(name.to_owned()).unwrap())
            .durable()
            .await
            .unwrap()
            .cancel(lash::CancelTarget::Run(
                lash_vocabulary::identity::RunId::parse("trace-suspended-cell-run").unwrap(),
            ))
            .mode(lash::TurnCancelMode::Immediate)
            .request_id("takeover-discard")
            .await
            .unwrap();
    } else {
        lash_core::waits::resolve_host(
            &world.backend,
            &key,
            lash_core::Resolution::Ok(serde_json::json!({ "answered": true })),
        )
        .await
        .expect("the deferred call's wait resolves");
    }
    drop(sent);
    let session = world
        .core
        .session(
            lash_vocabulary::identity::SessionId::try_from(name.to_owned()).expect("a session id"),
        )
        .durable()
        .await
        .expect("the session is open on the taking-over node");
    if discard {
        tokio::time::timeout(
            WATCHDOG,
            session
                .run(lash_vocabulary::identity::RunId::parse("trace-suspended-cell-run").unwrap())
                .output(),
        )
        .await
        .expect("the terminal owner answers")
        .expect("the cancelled turn settles");
    } else {
        let output = world.send(&session, "after the takeover").await;
        served::assert_answered("the follow-up turn", &output);
    }
    let mut violations = telemetry.first_turn_violations(name, false);
    violations.extend(telemetry.publication_violations(&[]));
    violations.extend(telemetry.first_turn_cell_violations(name, false));
    let closes = telemetry.first_turn_cell_closes(name);
    let code = "await tools.traceDeferred({});".to_owned();
    let published = telemetry.published();
    let closing_code: Vec<_> = published
        .iter()
        .filter_map(|record| {
            if record
                .context
                .session_id
                .as_ref()
                .is_none_or(|session| session.as_str() != name)
            {
                return None;
            }
            match &record.event {
                lash::tracing::TraceEvent::ExecCodeCompleted { code, .. }
                | lash::tracing::TraceEvent::ExecCodeDiscarded { code, .. } => Some(code.as_str()),
                _ => None,
            }
        })
        .collect();
    assert_eq!(
        closing_code,
        [code.as_str()],
        "the closing owner needs no opening record to export code"
    );
    if discard {
        let terminals = published
            .iter()
            .filter_map(|record| match &record.event {
                lash::tracing::TraceEvent::ToolCallCompleted {
                    name,
                    args,
                    output,
                    provider_call_id,
                    issuing_node_id,
                    attempts,
                    ..
                } => Some((
                    name,
                    args,
                    output,
                    provider_call_id,
                    issuing_node_id,
                    attempts,
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(terminals.len(), 1, "the abandoned call closes once");
        let (name, args, output, provider, node, attempts) = terminals[0];
        assert_eq!(
            name.name(),
            Some(DEFERRED_OPERATION),
            "the recorded tool name differs from its ToolId"
        );
        assert_eq!(args, &lash::tracing::TraceToolArgs::Unavailable);
        assert!(matches!(
            output.outcome,
            lash::tracing::TraceToolCallOutcome::Discarded
                | lash::tracing::TraceToolCallOutcome::Cancelled(_)
        ));
        assert!(provider.is_none() && node.is_none() && attempts.is_none());
    }
    let first_turn_input = published
        .iter()
        .find_map(|record| match &record.event {
            lash::tracing::TraceEvent::TurnCompleted { input, .. }
                if record
                    .context
                    .turn_id
                    .as_ref()
                    .is_some_and(|id| id.as_str() == "trace-suspended-cell-run") =>
            {
                Some(input)
            }
            _ => None,
        })
        .expect("the takeover exports the turn's closer");
    assert_eq!(
        serde_json::to_value(first_turn_input).unwrap()[0]["blocks"][0]["text"],
        name
    );
    world.shutdown().await;
    if !suspended.is_empty() {
        violations.push(format!(
            "the suspended cell closed before it answered: {suspended:?}"
        ));
    }
    if closes.iter().any(|close| close.started_at >= taken_over) {
        violations.push(format!(
            "the answered cell's span starts on the taking-over node, not at its first start \
             before {taken_over:?}: {closes:?}"
        ));
    }
    assert!(
        violations.is_empty(),
        "the suspended cell broke its one span:\n  {}",
        violations.join("\n  ")
    );
}

async fn a_suspended_cell_answered_on_a_takeover_node_is_one_span(tier: Tier) {
    suspended_cell_closes_on_takeover(tier, false).await;
}

/// A terminal owner restores only the retained cell start and checkpoint.
async fn a_suspended_cell_discarded_on_a_takeover_node_exports_its_code(tier: Tier) {
    suspended_cell_closes_on_takeover(tier, true).await;
}

/// The tool a code cell's call runs, which holds on the node the law
/// loses.
const HELD: &str = "trace_held";
const HELD_OPERATION: &str = "tools.trace_held";

/// The plugin whose before-turn callback holds on the node the law loses.
const HOLD_PLUGIN: &str = "trace-hold-law";

fn held_definition() -> lash_vocabulary::tool_contract::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_vocabulary::tool_contract::ToolDefinition::raw(
        format!("tool:{HELD}"),
        HELD_OPERATION,
        "Holds on the node the trace law loses.",
        object.clone(),
        object,
    )
    .expect("the tool's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    // A call its node lost while it ran runs again on the node that takes
    // the cell over, which builds its body without the candidate.
    .with_execution_policy(lash_vocabulary::tool_contract::ExecutionPolicy::repeatable(
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
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(vec![held_definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        (name == HELD_OPERATION).then(|| Arc::new(held_definition().contract()))
    }

    async fn execute(&self, _call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.hold().await;
        lash_core::ToolOutcome::ok(serde_json::json!({ "answered": true })).into()
    }
}

impl Hold {
    pub fn declaration() -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial(HOLD_PLUGIN)
    }
}
impl lash::plugins::PluginMetadata for Hold {
    fn plugin_declaration(&self) -> lash::plugins::PluginDeclaration {
        Self::declaration()
    }
}

impl lash::plugins::PluginFactory for Hold {
    fn id(&self) -> &'static str {
        HOLD_PLUGIN
    }

    fn recipe(self: Arc<Self>) -> Result<lash::plugins::PluginRecipe, lash::plugins::PluginError> {
        let mut reg = lash::plugins::PluginRegistrar::<Self>::new(
            lash_core::plugin::PluginFactory::id(self.as_ref()),
        );
        reg.turn().before(lash::hook_key!("hold"), |this, _| {
            let hold = this.clone();
            Ok(Arc::new(move |_| {
                let hold = hold.clone();
                Box::pin(async move {
                    hold.hold().await;
                    Ok(lash_core::plugin::TurnContributions::default())
                })
            }))
        })?;
        Ok(lash::plugins::PluginRecipe::new(
            HOLD_PLUGIN,
            reg,
            move |_, _| Ok(Arc::clone(&self)),
        ))
    }
}

impl lash::plugins::SessionPlugin for Hold {
    fn id(&self) -> &'static str {
        HOLD_PLUGIN
    }
}

/// Where the lost node holds.
#[derive(Clone, Copy, Debug)]
enum Lost {
    /// In the turn's before-turn callback: after `turn.admit` committed and
    /// its candidate was deferred, before `model.start`.
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
                served::rlm(backend, sim::untimed_workers()),
            )
            .plugin(packages::tools(
                lash::tool_provider_key!("tools"),
                Arc::new(hold),
            )),
        };
        builder.trace_runtime(telemetry.runtime())
    }
}

/// A turn whose node is lost where `lost` says, after the admission there,
/// and which a node of another process takes over, closes once, as does
/// its cell's call (FIG-5457, FIG-5998).
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
        world.script(&name, vec![served::cell("await tools.traceHeld({});")]);
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
    // the session over.
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
        .session(
            lash_vocabulary::identity::SessionId::try_from(name.clone()).expect("a session id"),
        )
        .durable()
        .await
        .expect("the session is open on the taking-over node");
    let output = world.send(&session, "after the takeover").await;
    served::assert_answered("the follow-up turn", &output);
    let mut violations = telemetry.first_turn_violations(&name, false);
    violations.extend(telemetry.publication_violations(&[]));
    if let Lost::InCellCall = lost {
        violations.extend(telemetry.first_turn_tool_violations(&name, false));
        violations.extend(telemetry.first_turn_cell_violations(&name, false));
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
    a_suspended_cell_answered_on_a_takeover_node_is_one_span,
    a_suspended_cell_discarded_on_a_takeover_node_exports_its_code,
);
