//! MCP event subscriptions on a served core (FIG-6178): each subscription
//! is a durable process of the MCP plugin's engine, polling or streaming a
//! SEP-3415 fixture server whose events outlive every server process and
//! every node.
//!
//! - SUBSCRIPTION-SURVIVES: a subscription a model made outlives its turn
//!   and its node, and the next node resumes it from its saved cursor.
//! - GAP-VISIBLE: a resume past the server's retention is a typed gap on
//!   the subscription's status and a notice turn, never silent.
//! - A degraded subscription's status shows its gap and park reason after a
//!   restart, and a resume clears the park.
//! - UNSUBSCRIBE-FENCES: once the unsubscribe commits no event reacts, and
//!   the open stream is cancelled at the server.
//! - WORKFLOW-TRIGGER-ONCE: each event starts its workflow once, however
//!   often the server redelivers it, and an event that does not map starts
//!   nothing and is a typed delivery failure.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use lash::mcp::McpPluginFactory;
use lash::mcp::events::testing::{FIXTURE_EVENT, McpEventsFixture};
use lash::mcp::events::{
    McpDeliveryError, McpDeliveryMode, McpEventBounds, McpEventsConfig, McpEventsError,
    McpInputMapping, McpMappingError, McpParkReason, McpReactionProfile, McpReactionTarget,
    McpSubscribeRequest, McpWorkflowReaction,
};
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::LlmRequest;
use serde_json::json;
use served::{Tier, WATCHDOG};

const SERVER: &str = "fx";
const SUBSCRIBE: &str = "subscribe to new tickets";
const WORKFLOW_ENGINE: &str = "mcp-events-law-workflow";

/// The model: it subscribes when asked to, and notes everything else.
fn model() -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("mcp-events-model")
        .requires_streaming(true)
        .complete(|request: LlmRequest| async move {
            let seen = format!("{:?}", request.messages);
            if seen.contains(SUBSCRIBE) && !seen.contains("subscription_id") {
                return Ok(served::response(vec![served::call(
                    "subscribe-call",
                    "subscribe_event",
                    json!({
                        "server": SERVER,
                        "event": FIXTURE_EVENT,
                        "reaction": { "kind": "session" },
                    }),
                )]));
            }
            Ok(served::text(&request, "noted"))
        })
        .build()
        .into_handle()
}

fn events(mode: McpDeliveryMode) -> McpEventsConfig {
    McpEventsConfig::new([mode])
        .with_reactions(McpReactionProfile {
            own_session: true,
            ..McpReactionProfile::default()
        })
        .with_bounds(McpEventBounds {
            poll_floor_ms: 20,
            push_window_ms: 2_000,
            retry_initial_ms: 20,
            retry_max_ms: 100,
            failures_before_park: 1,
            ..McpEventBounds::default()
        })
}

/// A deployment over one database whose node dies and is replaced, each
/// node with its own MCP plugin over the one fixture world.
struct Deployment {
    fixture: McpEventsFixture,
    mode: McpDeliveryMode,
    backend: lash::Backend,
    core: lash::LashCore,
    plugin: Arc<McpPluginFactory>,
    _world: tempfile::TempDir,
    _keep: served::Keep,
}

impl Deployment {
    async fn new(tier: Tier, mode: McpDeliveryMode, control: serde_json::Value) -> Option<Self> {
        let (stores, keep) = served::stores(tier).await?;
        let workflow: Arc<dyn lash_core::ProcessEngine> = Arc::new(LawWorkflow);
        let backend = served::backend_with(stores, vec![workflow]);
        let world = tempfile::tempdir().expect("the fixture's world");
        let fixture = McpEventsFixture::new(world.path());
        fixture.control(control);
        let (core, plugin) = Self::serve(&backend, &fixture, mode, "first-boot").await;
        Some(Self {
            fixture,
            mode,
            backend,
            core,
            plugin,
            _world: world,
            _keep: keep,
        })
    }

    async fn serve(
        backend: &lash::Backend,
        fixture: &McpEventsFixture,
        mode: McpDeliveryMode,
        boot: &str,
    ) -> (lash::LashCore, Arc<McpPluginFactory>) {
        let plugin = Arc::new(
            McpPluginFactory::new(BTreeMap::from([(
                SERVER.to_owned(),
                fixture.server(events(mode)),
            )]))
            .await
            .expect("the MCP plugin builds"),
        );
        let core = core(backend, &plugin, boot);
        core.bind_mcp_events(&plugin.events());
        (core, plugin)
    }

    /// The node dies; a new one, with its own plugin and server process,
    /// serves the same database in its place.
    async fn restart(&mut self, boot: &str) {
        self.core.shutdown().await.expect("the core shuts down");
        let (core, plugin) = Self::serve(&self.backend, &self.fixture, self.mode, boot).await;
        self.core = core;
        self.plugin = plugin;
    }

    async fn session(&self, name: &str) -> lash::SessionId {
        let id = lash::SessionId::try_from(name.to_owned()).expect("a session id");
        self.core
            .session(id.clone())
            .create(lash::SessionCreation::root(served::spec(8)))
            .await
            .expect("the session is created");
        id
    }

    /// A host subscription of `session`, reacting in it.
    async fn subscribe(&self, session: &lash::SessionId, key: &str) -> lash::ProcessId {
        self.plugin
            .events()
            .subscribe(
                McpSubscribeRequest::host(
                    SERVER,
                    FIXTURE_EVENT,
                    serde_json::Map::new(),
                    McpReactionTarget::Session {
                        session: session.clone(),
                    },
                    key,
                )
                .with_observers([session.clone()]),
            )
            .await
            .expect("the subscription starts")
    }

    /// Every text `session`'s committed transcript holds.
    async fn transcript(&self, session: &lash::SessionId) -> String {
        let Some(view) = self
            .core
            .session(session.clone())
            .durable()
            .await
            .expect("the durable session")
            .read()
            .await
            .expect("the committed read")
        else {
            return String::new();
        };
        view.messages()
            .iter()
            .flat_map(|message| message.parts.iter())
            .map(|part| part.content().into_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// How many turns of `session` reacted to event `id`.
    async fn reactions(&self, session: &lash::SessionId, id: &str) -> usize {
        self.transcript(session)
            .await
            .matches(&format!("\"eventId\": \"{id}\""))
            .count()
    }

    /// The subscription's status, once its first transition recorded one.
    async fn status(&self, id: &lash::ProcessId) -> lash::mcp::events::McpSubscriptionStatus {
        tokio::time::timeout(WATCHDOG, async {
            loop {
                let status = self
                    .plugin
                    .events()
                    .status(id)
                    .await
                    .expect("the subscription's status");
                if let Some(status) = status {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("deadlock watchdog: the subscription never recorded a status")
    }

    async fn shutdown(self) {
        self.core.shutdown().await.expect("the core shuts down");
    }
}

/// A core over `backend` with `plugin`, serving its own node as `boot`.
fn core(backend: &lash::Backend, plugin: &Arc<McpPluginFactory>, boot: &str) -> lash::LashCore {
    lash::LashCore::standard_builder(backend.clone())
        .plugin(Arc::clone(plugin) as Arc<dyn lash::plugins::PluginFactory>)
        .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(model(), served::metadata())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("mcp-events-deployment"),
            lash::persistence::LeaseIncarnationId::new(boot),
        ))
        .expect("the core builds")
}

/// Wait until `subscription`'s status satisfies `check`, within the
/// watchdog; a timeout names the last status.
async fn status_until(
    deployment: &Deployment,
    subscription: &lash::ProcessId,
    what: &str,
    check: impl Fn(&lash::mcp::events::McpSubscriptionStatus) -> bool,
) {
    let deadline = tokio::time::Instant::now() + WATCHDOG;
    loop {
        let status = deployment.status(subscription).await;
        if check(&status) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "deadlock watchdog: {what} never held; the last status: {status:?}; the \
             subscription: {:?}",
            deployment.core.processes().get(subscription).await
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Wait until `check` holds, within the deadlock watchdog.
async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    tokio::time::timeout(WATCHDOG, async {
        while !check().await {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("deadlock watchdog: {what} never held"));
}

/// SUBSCRIPTION-SURVIVES: the model subscribes in a turn that then ends;
/// an event reacts; the node dies and another takes over with a new server
/// process; the next event reacts there, once, from the saved cursor.
async fn a_subscription_outlives_its_turn_and_its_node(tier: Tier) {
    let Some(mut deployment) =
        Deployment::new(tier, McpDeliveryMode::Poll, json!({ "next_poll_ms": 20 })).await
    else {
        return;
    };
    let session = deployment.session("subscriber").await;
    let durable = deployment
        .core
        .session(session.clone())
        .durable()
        .await
        .expect("the durable session");
    let subscribed = tokio::time::timeout(
        WATCHDOG,
        durable.send(lash::TurnInput::text(SUBSCRIBE)).output(),
    )
    .await
    .expect("the subscribing turn settles")
    .expect("the subscribing turn answers");
    served::assert_answered("the subscribing turn", &subscribed);
    let listed = deployment
        .plugin
        .events()
        .list(Some(&session))
        .await
        .expect("the subscriptions list");
    assert_eq!(
        listed.len(),
        1,
        "the turn made one subscription: {listed:?}"
    );
    // The first poll starts from now: what the server holds before it is
    // not this subscription's.
    eventually("the first poll", || async {
        !deployment.fixture.requests_of("events/poll").is_empty()
    })
    .await;
    deployment.fixture.emit("e1", json!({ "ticket": 1 }));
    eventually("the reaction to e1", || async {
        deployment.reactions(&session, "e1").await == 1
    })
    .await;

    deployment.restart("second-boot").await;
    deployment.fixture.emit("e2", json!({ "ticket": 2 }));
    eventually("the reaction to e2 on the new node", || async {
        deployment.reactions(&session, "e2").await == 1
    })
    .await;
    assert_eq!(deployment.reactions(&session, "e1").await, 1);
    let polls = deployment.fixture.requests_of("events/poll");
    let fresh = polls
        .iter()
        .filter(|poll| poll["params"]["cursor"].is_null())
        .count();
    assert_eq!(
        fresh, 1,
        "only the subscription's first poll starts from now; the new node resumed from the \
         saved cursor"
    );
    let status = deployment.status(&listed[0].id).await;
    assert_eq!(status.delivered, 2, "{status:?}");
    assert!(!status.degraded(), "{status:?}");
    deployment.shutdown().await;
}

/// GAP-VISIBLE: the server resumes later than the saved cursor
/// (`truncated`); the subscription records a typed gap and its session gets
/// a notice turn, and the skipped event is never claimed.
async fn a_resume_past_retention_is_a_recorded_gap_and_a_notice(tier: Tier) {
    let Some(deployment) =
        Deployment::new(tier, McpDeliveryMode::Poll, json!({ "next_poll_ms": 20 })).await
    else {
        return;
    };
    let session = deployment.session("gapped").await;
    let subscription = deployment.subscribe(&session, "gapped-subscription").await;
    eventually("the first poll", || async {
        !deployment.fixture.requests_of("events/poll").is_empty()
    })
    .await;
    deployment.fixture.emit("e1", json!({}));
    eventually("the reaction to e1", || async {
        deployment.reactions(&session, "e1").await == 1
    })
    .await;
    // The server drops e2 from its retention before the next poll.
    deployment
        .fixture
        .control(json!({ "next_poll_ms": 20, "retained_from": 2 }));
    deployment.fixture.emit("e2", json!({}));
    deployment.fixture.emit("e3", json!({}));
    eventually("the reaction to e3", || async {
        deployment.reactions(&session, "e3").await == 1
    })
    .await;
    let status = deployment.status(&subscription).await;
    assert_eq!(status.gaps, 1, "{status:?}");
    assert!(status.degraded());
    assert!(status.recent_gaps[0].had_cursor, "{status:?}");
    let transcript = deployment.transcript(&session).await;
    assert_eq!(
        transcript.matches("reports a gap").count(),
        1,
        "{transcript}"
    );
    assert_eq!(deployment.reactions(&session, "e2").await, 0);
    deployment.shutdown().await;
}

/// A degraded subscription keeps its typed gap and park reason on record
/// across a node restart; a resume clears the park and it delivers again.
async fn a_parked_subscription_keeps_its_status_across_a_restart_and_resumes(tier: Tier) {
    let Some(mut deployment) = Deployment::new(
        tier,
        McpDeliveryMode::Poll,
        json!({ "next_poll_ms": 20, "null_cursor": "origin", "retained_from": 1 }),
    )
    .await
    else {
        return;
    };
    let session = deployment.session("parked").await;
    deployment.fixture.emit("e1", json!({}));
    deployment.fixture.emit("e2", json!({}));
    let subscription = deployment.subscribe(&session, "parked-subscription").await;
    eventually("the reaction to e2", || async {
        deployment.reactions(&session, "e2").await == 1
    })
    .await;
    deployment.fixture.control(json!({
        "next_poll_ms": 20,
        "refuse": { "code": -32024, "message": "access revoked" },
    }));
    eventually("the park", || async {
        deployment.status(&subscription).await.parked.is_some()
    })
    .await;

    deployment.restart("second-boot").await;
    let status = deployment.status(&subscription).await;
    assert_eq!(status.gaps, 1, "{status:?}");
    assert!(!status.recent_gaps[0].had_cursor, "{status:?}");
    match &status.parked {
        Some(McpParkReason::Forbidden { error }) => assert_eq!(error.code, -32024),
        other => panic!("parked for the wrong reason: {other:?}"),
    }

    deployment.fixture.control(json!({ "next_poll_ms": 20 }));
    deployment.fixture.emit("e3", json!({}));
    assert!(
        deployment
            .plugin
            .events()
            .resume(&subscription)
            .await
            .expect("the resume")
    );
    eventually("the reaction to e3 after the resume", || async {
        deployment.reactions(&session, "e3").await == 1
    })
    .await;
    let status = deployment.status(&subscription).await;
    assert_eq!(status.parked, None, "{status:?}");
    assert_eq!(status.gaps, 1, "the gap stays on record: {status:?}");
    assert_eq!(deployment.reactions(&session, "e1").await, 0);
    deployment.shutdown().await;
}

/// UNSUBSCRIBE-FENCES, on a push stream: an event reacts; the unsubscribe
/// commits; a later event never reacts, and every stream the subscription
/// opened was cancelled at the server.
async fn an_unsubscribe_fences_every_later_event(tier: Tier) {
    let Some(deployment) =
        Deployment::new(tier, McpDeliveryMode::Push, json!({ "heartbeat_ms": 100 })).await
    else {
        return;
    };
    let session = deployment.session("unsubscribed").await;
    let subscription = deployment
        .subscribe(&session, "unsubscribed-subscription")
        .await;
    eventually("the first stream", || async {
        !deployment.fixture.requests_of("events/stream").is_empty()
    })
    .await;
    deployment.fixture.emit("e1", json!({}));
    eventually("the reaction to e1", || async {
        deployment.reactions(&session, "e1").await == 1
    })
    .await;
    deployment
        .plugin
        .events()
        .unsubscribe(&subscription)
        .await
        .expect("the unsubscribe");
    tokio::time::timeout(
        WATCHDOG,
        deployment.core.processes().await_output(&subscription),
    )
    .await
    .expect("the subscription ends")
    .expect("its terminal");
    let streams = deployment.fixture.requests_of("events/stream").len();
    deployment.fixture.emit("e2", json!({}));
    // Many windows' worth of time: a live subscription would have reacted.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(deployment.reactions(&session, "e2").await, 0);
    assert_eq!(
        deployment.fixture.requests_of("events/stream").len(),
        streams,
        "no stream opens after the unsubscribe"
    );
    let cancelled: Vec<_> = deployment
        .fixture
        .requests_of("notifications/cancelled")
        .into_iter()
        .map(|notification| notification["params"]["requestId"].clone())
        .collect();
    for stream in deployment.fixture.requests_of("events/stream") {
        assert!(
            cancelled.contains(&stream["id"]),
            "stream {} was never cancelled at the server",
            stream["id"]
        );
    }
    deployment.shutdown().await;
}

/// WORKFLOW-TRIGGER-ONCE: a host registers a workflow trigger; the server
/// delivers every event twice; each event that maps starts its workflow
/// once, and one that does not starts nothing and is a typed failure.
async fn a_workflow_trigger_starts_once_per_event(tier: Tier) {
    let Some(deployment) = Deployment::new(
        tier,
        McpDeliveryMode::Poll,
        json!({ "next_poll_ms": 20, "null_cursor": "origin" }),
    )
    .await
    else {
        return;
    };
    let pin = lash::process::HostArtifactPin::mint();
    let artifacts = deployment.core.host_artifacts();
    let definition = artifacts
        .publish_definition(
            &pin,
            &lash_core::ProcessDefinitionDraft::new(
                WORKFLOW_ENGINE,
                json!({ "workflow": "triage" }),
                [],
            )
            .expect("the draft"),
        )
        .await
        .expect("the definition publishes");
    let env = artifacts
        .publish_process_env(
            &pin,
            &lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(8))
                .build()
                .expect("the environment"),
        )
        .await
        .expect("the environment publishes");
    let reaction = |fixed: serde_json::Value| McpWorkflowReaction {
        definition: definition.clone(),
        launch: lash::process::EngineLaunch::new(env.clone(), None),
        mapping: McpInputMapping {
            fixed: fixed.as_object().cloned().unwrap_or_default(),
            from_event: BTreeMap::from([("incident".to_owned(), "/data/incident".to_owned())]),
        },
    };
    let trigger = |fixed: serde_json::Value, key: &str| {
        McpSubscribeRequest::host(
            SERVER,
            FIXTURE_EVENT,
            serde_json::Map::new(),
            McpReactionTarget::Workflow(reaction(fixed)),
            key,
        )
    };
    // Registration checks the fixed arguments against the definition.
    match deployment
        .plugin
        .events()
        .subscribe(trigger(json!({ "priority": 1 }), "refused-trigger"))
        .await
    {
        Err(McpEventsError::Arguments { code, .. }) => assert_eq!(code, "argument_invalid"),
        other => panic!("a misfitting trigger registered: {other:?}"),
    }
    deployment.fixture.emit("e1", json!({ "incident": "i-1" }));
    deployment
        .fixture
        .emit("e2", json!({ "summary": "no incident" }));
    deployment.fixture.emit("e3", json!({ "incident": "i-3" }));
    let subscription = deployment
        .plugin
        .events()
        .subscribe(trigger(json!({ "source": "fx" }), "triage-trigger"))
        .await
        .expect("the trigger registers");
    status_until(&deployment, &subscription, "every delivery", |status| {
        status.delivered >= 2 && status.delivery_failures >= 1
    })
    .await;
    // The server delivers all three again.
    deployment
        .fixture
        .control(json!({ "next_poll_ms": 20, "null_cursor": "origin", "rewind_once": 3 }));
    status_until(
        &deployment,
        &subscription,
        "every delivery, twice",
        |status| status.delivered >= 4 && status.delivery_failures >= 2,
    )
    .await;
    let filter = lash_core::ProcessListFilter {
        definition_id: Some(definition.id.clone()),
        status: lash_core::ProcessStatusFilter::Any,
        ..lash_core::ProcessListFilter::default()
    };
    let started = deployment
        .core
        .processes()
        .list(&filter, std::num::NonZeroUsize::new(64).unwrap(), None)
        .await
        .expect("the workflow runs list")
        .processes;
    assert_eq!(started.len(), 2, "one run per mapped event: {started:?}");
    let status = deployment.status(&subscription).await;
    assert!(status.degraded(), "{status:?}");
    for failure in &status.recent_delivery_failures {
        assert_eq!(failure.item, "event:e2");
        assert!(
            matches!(
                &failure.error,
                McpDeliveryError::Mapping(McpMappingError::Missing { argument, .. })
                    if argument == "incident"
            ),
            "{failure:?}"
        );
    }
    deployment.shutdown().await;
}

/// OFF-BY-DEFAULT: a server that does not enable events changes nothing:
/// its configuration serializes as before, the plugin registers what it
/// registers with no server at all (no event tool, no engine), and a
/// subscribe is refused. A server that enables them adds both.
async fn events_are_off_unless_a_server_enables_them(tier: Tier) {
    let Some((stores, _keep)) = served::stores(tier).await else {
        return;
    };
    let backend = served::backend(stores);
    let world = tempfile::tempdir().expect("the fixture's world");
    let fixture = McpEventsFixture::new(world.path());
    let mut disabled = fixture.server(events(McpDeliveryMode::Poll));
    disabled.events = lash::mcp::events::McpServerEvents::Disabled;
    let shape = serde_json::to_value(&disabled).expect("the configuration serializes");
    assert!(shape.get("events").is_none(), "{shape}");
    let read: lash::mcp::McpServerConfig =
        serde_json::from_value(shape).expect("a configuration without events reads");
    assert!(read.events.is_disabled());

    let inventory = |plugin: Arc<McpPluginFactory>, boot: &'static str| {
        let backend = backend.clone();
        async move {
            let core = core(&backend, &plugin, boot);
            core.bind_mcp_events(&plugin.events());
            let installed = match core.plugin_diagnostics().installed(
                &lash::plugins::InventoryScope::Deployment(lash::plugins::RegistryKind::Session),
            ) {
                lash::plugins::diagnostics::InstalledObservation::Present(inventory) => {
                    inventory.entries
                }
                other => panic!("the inventory is unavailable: {other:?}"),
            };
            let engine = core
                .processes()
                .engine_defaults(
                    lash::mcp::events::MCP_EVENTS_ENGINE,
                    &serde_json::Value::Null,
                )
                .is_ok();
            let refused = plugin
                .events()
                .subscribe(McpSubscribeRequest::host(
                    SERVER,
                    FIXTURE_EVENT,
                    serde_json::Map::new(),
                    McpReactionTarget::Session {
                        session: lash::SessionId::try_from("nobody".to_owned()).unwrap(),
                    },
                    "off-by-default",
                ))
                .await
                .err();
            core.shutdown().await.expect("the core shuts down");
            (format!("{installed:?}"), engine, refused)
        }
    };
    let none = McpPluginFactory::new(BTreeMap::new())
        .await
        .expect("the plugin builds");
    let (baseline, _, _) = inventory(Arc::new(none), "no-server").await;
    let off = McpPluginFactory::new(BTreeMap::from([(SERVER.to_owned(), disabled)]))
        .await
        .expect("the plugin builds");
    let (installed, engine, refused) = inventory(Arc::new(off), "events-off").await;
    assert_eq!(
        installed, baseline,
        "a server without events registers nothing new"
    );
    assert!(!engine, "no subscription engine is registered");
    assert!(
        matches!(
            refused,
            Some(McpEventsError::Server(
                lash::mcp::events::McpEventsCallError::Disabled { .. }
            ))
        ),
        "{refused:?}"
    );
    for tool in [
        "list_events",
        "subscribe_event",
        "list_subscriptions",
        "unsubscribe",
    ] {
        assert!(
            !installed.contains(tool),
            "`{tool}` is offered: {installed}"
        );
    }

    let on = McpPluginFactory::new(BTreeMap::from([(
        SERVER.to_owned(),
        fixture.server(events(McpDeliveryMode::Poll)),
    )]))
    .await
    .expect("the plugin builds");
    let (installed, engine, _) = inventory(Arc::new(on), "events-on").await;
    assert_ne!(
        installed, baseline,
        "an enabling server adds the event tools"
    );
    assert!(engine, "an enabling server adds the subscription engine");
}

/// An isolated reaction: each event runs as the first turn of a child
/// session of its own, once, however often the server delivers it.
async fn an_isolated_reaction_runs_each_event_in_its_own_child_once(tier: Tier) {
    let Some(deployment) = Deployment::new(
        tier,
        McpDeliveryMode::Poll,
        json!({ "next_poll_ms": 20, "null_cursor": "origin" }),
    )
    .await
    else {
        return;
    };
    deployment.fixture.emit("e1", json!({}));
    deployment.fixture.emit("e2", json!({}));
    let child = lash::SessionCreateRequest::root(
        lash_core::SessionStartPoint::Empty,
        lash_core::PluginOptions::default(),
    )
    .with_spec(&served::spec(8))
    .expect("the child's spec");
    let subscription = deployment
        .plugin
        .events()
        .subscribe(McpSubscribeRequest::host(
            SERVER,
            FIXTURE_EVENT,
            serde_json::Map::new(),
            McpReactionTarget::Isolated {
                child: Box::new(child),
            },
            "isolated-subscription",
        ))
        .await
        .expect("the subscription starts");
    status_until(&deployment, &subscription, "both deliveries", |status| {
        status.delivered >= 2
    })
    .await;
    deployment
        .fixture
        .control(json!({ "next_poll_ms": 20, "null_cursor": "origin", "rewind_once": 2 }));
    status_until(
        &deployment,
        &subscription,
        "both deliveries, twice",
        |status| status.delivered >= 4,
    )
    .await;
    let filter = lash_core::ProcessListFilter {
        originator: Some(lash_core::ProcessOriginatorFilter::Host {
            scope: Some(format!("mcp-events:{subscription}")),
        }),
        status: lash_core::ProcessStatusFilter::Any,
        ..lash_core::ProcessListFilter::default()
    };
    let reactions = deployment
        .core
        .processes()
        .list(&filter, std::num::NonZeroUsize::new(64).unwrap(), None)
        .await
        .expect("the reactions list")
        .processes;
    assert_eq!(
        reactions.len(),
        2,
        "one child turn per event: {reactions:?}"
    );
    let children: std::collections::BTreeSet<_> = reactions
        .iter()
        .map(|reaction| reaction.child_session_id.clone())
        .collect();
    assert_eq!(
        children.len(),
        2,
        "each event has a child of its own: {reactions:?}"
    );
    for reaction in reactions {
        let output = tokio::time::timeout(
            WATCHDOG,
            deployment
                .core
                .processes()
                .await_output(&reaction.process_id),
        )
        .await
        .expect("the child turn ends")
        .expect("its output");
        let answer = serde_json::to_string(&output).expect("the output serializes");
        assert!(answer.contains("noted"), "{answer}");
    }
    deployment.shutdown().await;
}

/// The workflow a trigger starts: its signature takes `incident` (required)
/// and `source`; a run ends at once.
struct LawWorkflow;

#[async_trait::async_trait]
impl lash_core::ProcessEngine for LawWorkflow {
    fn kind(&self) -> &'static str {
        WORKFLOW_ENGINE
    }

    fn state_format(&self) -> lash_core::EngineStateFormat {
        lash_core::EngineStateFormat {
            kind: WORKFLOW_ENGINE.to_owned(),
            version: 0,
        }
    }

    fn cancel_grace(&self) -> Duration {
        Duration::ZERO
    }

    fn program_identity(
        &self,
        _payload: &serde_json::Value,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    fn check_settings(
        &self,
        _payload: &serde_json::Value,
        _settings: Option<&lash_core::EngineSettings>,
    ) -> Result<(), lash_core::PluginError> {
        Ok(())
    }

    fn default_settings(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Option<lash_core::EngineSettings>, lash_core::PluginError> {
        Ok(None)
    }

    fn advance(
        &self,
        state: lash_core::EngineState,
        event: lash_core::EngineEvent,
    ) -> Result<(lash_core::EngineState, lash_core::EngineAction), lash_core::ProcessInfraError>
    {
        let action = match event {
            lash_core::EngineEvent::Started { payload } => {
                lash_core::EngineAction::Terminal(lash_core::ProcessOutcome::from_tool_output(
                    lash_core::ToolCallOutput::success(payload),
                ))
            }
            _ => lash_core::EngineAction::Idle,
        };
        Ok((state, action))
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Vec<lash_core::ArtifactName>, lash_core::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), lash_core::PluginError> {
        Err(lash_core::PluginError::Session(format!(
            "the law workflow stores no artifact `{artifact_ref}`"
        )))
    }

    async fn performed_effects(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<lash_core::PerformedEffects, lash_core::ProcessEnvRefusal> {
        Ok(lash_core::PerformedEffects::new())
    }

    async fn check_args(
        &self,
        _signature: &lash_core::ProcessSignature,
        args: &serde_json::Map<String, serde_json::Value>,
        mode: lash_core::ArgsMode,
    ) -> Result<(), lash_core::ArgsMismatch> {
        if let Some(path) = args
            .keys()
            .find(|name| !matches!(name.as_str(), "incident" | "source"))
        {
            return Err(lash_core::ArgsMismatch::Argument {
                path: path.clone(),
                message: "the workflow takes no such argument".to_owned(),
            });
        }
        if mode == lash_core::ArgsMode::Complete && !args.contains_key("incident") {
            return Err(lash_core::ArgsMismatch::ArgumentMissing {
                path: "incident".to_owned(),
            });
        }
        Ok(())
    }

    async fn resolve(
        &self,
        _reference: &lash_core::ProcessDefinitionRef,
    ) -> Result<lash_core::ProcessDefinitionResolution, lash_core::ProcessDefinitionError> {
        Ok(lash_core::ProcessDefinitionResolution::new(
            lash_core::ProcessSignature::Unknown,
        ))
    }
}

/// The laws on the SQLite tier: the fixture server is a local process, and
/// what they pin is the plugin's, the same over every store.
macro_rules! sqlite_laws {
    ($($law:ident),+ $(,)?) => {
        mod sqlite_memory {
            $(
                #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
                async fn $law() {
                    super::$law(super::served::Tier::SqliteMemory).await;
                }
            )+
        }
    };
}

sqlite_laws!(
    events_are_off_unless_a_server_enables_them,
    a_subscription_outlives_its_turn_and_its_node,
    a_resume_past_retention_is_a_recorded_gap_and_a_notice,
    a_parked_subscription_keeps_its_status_across_a_restart_and_resumes,
    an_unsubscribe_fences_every_later_event,
    a_workflow_trigger_starts_once_per_event,
    an_isolated_reaction_runs_each_event_in_its_own_child_once,
);
