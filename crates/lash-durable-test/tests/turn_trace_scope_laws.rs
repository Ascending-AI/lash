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
//!   back instead of admitting the call again.
//!
//! The crash half, a turn cut at a phase commit and resumed on the other
//! node, is `Turn::Trace` in `tool_crash_laws.rs`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;
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
        world
            .restart("trace-takeover-boot", builder(&telemetry, &keys))
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

tiered_laws!(
    a_turn_resumed_on_its_node_keeps_its_trace_scope,
    a_turn_resumed_on_a_takeover_node_keeps_its_trace_scope,
);
