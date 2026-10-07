//! A durable turn's tool activity and presentation through a host's
//! `send()` on a served node (FIG-5251).
//!
//! A host creates a session on a core over the tier's database and sends it
//! an input; the core's own node claims the session and runs the turn on
//! the durable path with the law's scripted model and host tools.
//!
//! - **Activity:** a native call's `ToolCallStarted` reaches the host's
//!   sink live, while the call's body still holds, so before the round's
//!   outcome commit; its `ToolIntentOutcome` and `ToolCallCompleted` follow
//!   once the call settles. The live stream is provisional (ADR 0002): the
//!   turn's commit settles it, and only a turn's terminal waits for that
//!   commit (ADR 0122). The crash half, one committed outcome per call
//!   after a cut at `round.outcome`, is in `tool_crash_laws.rs`.
//! - **Presentation:** a plugin's presentation step presents every native
//!   round call, one that answers at once and one that parks and is
//!   resolved out of band, and its output is what the turn records and the
//!   model is shown.
//! - **Commits:** a host following the session through its recoverable
//!   chat sees the turn's provisional activity settled by exactly one
//!   `TerminalReplacement` per commit, carrying the rows the commit added,
//!   also when another node made the commit. The crash half, a commit whose
//!   publication was lost, is in `tool_crash_laws.rs`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;

use std::sync::{Arc, Mutex};

use lash::observe::Stream as _;
use lash::recoverable_chat::{RecoverableChatSubscription, RecoverableChatUpdate};
use lash_core::ToolDefinitionBindingExt as _;
use lash_core::{
    LiveReplayStore, SessionObservationEventPayload, ToolCall, TurnActivity, TurnEvent,
};
use lash_sansio::sync::MutexExt as _;

use served::{Tier, WATCHDOG, World};

/// The tool whose body holds on the law's gate, then answers and, when
/// asked, declares one detached child.
const GATED: &str = "activity_gated";
/// The tool that parks on its completion key; the law's gate resolves it.
const DEFERRED: &str = "activity_deferred";
/// What the law's presentation step appends to every call's return.
const PRESENTED: &str = "presented by the activity law's step";

/// A gate a tool body waits on until the law opens it.
#[derive(Clone)]
struct Gate(Arc<tokio::sync::watch::Sender<bool>>);

impl Gate {
    fn new(open: bool) -> Self {
        Self(Arc::new(tokio::sync::watch::channel(open).0))
    }

    fn open(&self) {
        self.0.send_replace(true);
    }

    async fn passed(&self) {
        let mut open = self.0.subscribe();
        let _ = open.wait_for(|open| *open).await;
    }
}

fn definition(name: &str) -> lash_core::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    let definition = lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "Answers its label once the activity law's gate opens.",
        object.clone(),
        object,
    )
    .expect("the tool's schemas")
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], name));
    if name == DEFERRED {
        definition.with_declaration(lash_core::ToolDeclaration::deferring())
    } else {
        definition.with_declaration(
            lash_core::ToolDeclaration::default()
                .with_intents([lash_core::ToolIntentKind::StartProcess]),
        )
    }
}

/// The law's tools: both hold on `gate`.
struct ActivityTools {
    gate: Gate,
    backend: lash::Backend,
}

impl ActivityTools {
    /// One detached held child of the calling session's frame.
    fn start(
        call: &ToolCall<'_>,
        label: &str,
    ) -> Result<lash_core::StartProcessIntent, lash_core::PluginError> {
        let session_id = call.context.session_id()?.clone();
        let agent_frame_id = call.context.agent_frame_id()?.clone();
        let declaration = lash_core::ProcessStartDeclaration::new(
            lash_core_execution::testing::held_engine_input(
                serde_json::json!({ "activity-law": label }),
            ),
            lash_core::ProcessOriginator::Session {
                session_id: session_id.clone(),
                agent_frame_id: Some(agent_frame_id),
            },
            lash_core::Lifetime::Detached,
        )
        .with_env_ref(call.context.process_execution_env_ref()?);
        Ok(lash_core::StartProcessIntent {
            owner: lash_core::RuntimeOwner::Session(session_id),
            declaration,
        })
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for ActivityTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        [GATED, DEFERRED]
            .into_iter()
            .map(|name| definition(name).manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        [GATED, DEFERRED]
            .contains(&name)
            .then(|| Arc::new(definition(name).contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let label = call.args["label"].as_str().unwrap_or_default().to_owned();
        if call.name() == DEFERRED {
            let key = call
                .context
                .completion_key()
                .expect("a deferring call's round pins its completion wait");
            let gate = self.gate.clone();
            let backend = self.backend.clone();
            tokio::spawn(async move {
                gate.passed().await;
                lash_core::waits::resolve_host(
                    &backend,
                    key.as_str(),
                    lash_core::Resolution::Ok(serde_json::json!({ "answered": label })),
                )
                .await
                .expect("the deferred call's wait resolves");
            });
            return lash_core::ToolAttemptOutcome::Pending(lash_core::PendingCompletion::new());
        }
        self.gate.passed().await;
        let answer = lash_core::ToolOutcomeDone::ok(serde_json::json!({ "answered": label }));
        if !call.args["start"].as_bool().unwrap_or_default() {
            return lash_core::ToolAttemptOutcome::done(answer, lash_core::ToolIntents::default());
        }
        match Self::start(&call, &label) {
            Ok(start) => lash_core::ToolAttemptOutcome::done(
                answer,
                lash_core::ToolIntents::v3(vec![lash_core::ToolIntent::StartProcess(Box::new(
                    start,
                ))]),
            ),
            Err(error) => lash_core::ToolOutcome::err_fmt(error).into(),
        }
    }
}

/// A plugin whose one presentation step appends [`PRESENTED`] to every
/// call's model-facing return.
fn presenter() -> Arc<dyn lash_core::facade_support::PluginFactory> {
    let step: lash_core::plugin::ToolPresentationStep = Arc::new(|input| {
        Box::pin(async move {
            let mut presented = input.previous;
            presented
                .parts
                .push(lash_core::facade_support::ModelToolReturnPart::text(
                    PRESENTED,
                ));
            Ok(presented)
        })
    });
    Arc::new(lash_core::plugin::StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("activity-law-presenter"),
        lash_core::facade_support::PluginSpec::new()
            .with_presentation_step(lash_core::hook_key!("activity-law-present"), step),
    ))
}

/// A core serving the law's tools behind `gate`, with the presenter when
/// `present`.
async fn world(tier: Tier, gate: &Gate, present: bool) -> Option<World> {
    world_with(tier, gate, present, None).await
}

/// [`world`] publishing to `live`, a live replay store other cores share,
/// when one is given.
async fn world_with(
    tier: Tier,
    gate: &Gate,
    present: bool,
    live: Option<Arc<dyn LiveReplayStore>>,
) -> Option<World> {
    let gate = gate.clone();
    World::with_engines(
        tier,
        vec![Arc::new(lash_core_execution::testing::HeldProcessEngine)],
        move |backend| {
            let mut builder =
                lash::LashCore::standard_builder(backend.clone()).tools(Arc::new(ActivityTools {
                    gate,
                    backend: backend.clone(),
                }));
            if present {
                builder = builder.plugin(presenter());
            }
            match live {
                Some(live) => builder.live_replay_store(live),
                None => builder,
            }
        },
    )
    .await
}

/// The next update of `updates`, within the watchdog.
async fn next_update(updates: &mut RecoverableChatSubscription) -> RecoverableChatUpdate {
    tokio::time::timeout(
        WATCHDOG,
        std::future::poll_fn(|cx| std::pin::Pin::new(&mut *updates).poll_next(cx)),
    )
    .await
    .expect("deadlock watchdog: the recoverable chat yielded nothing")
    .expect("the recoverable chat stays open")
    .expect("the recoverable chat answers")
}

/// The updates of `updates` up to and including the next
/// `TerminalReplacement`.
async fn through_next_commit(
    updates: &mut RecoverableChatSubscription,
) -> Vec<RecoverableChatUpdate> {
    let mut seen = Vec::new();
    loop {
        let update = next_update(updates).await;
        let commit = matches!(update, RecoverableChatUpdate::TerminalReplacement { .. });
        seen.push(update);
        if commit {
            return seen;
        }
    }
}

/// The updates of `updates` while the call `provider_call_id` holds on
/// `gate`, up to and including its `ToolCallStarted`; then, with the gate
/// open, up to and including the next `TerminalReplacement`.
///
/// A subscription attaches on its first poll. A subscriber first polled
/// once the turn ended meets its commit durable but perhaps not yet
/// published, which is a gap. Held behind the gate, the turn cannot commit
/// before the subscription attached and streamed the call's start.
async fn through_the_held_call_s_commit(
    updates: &mut RecoverableChatSubscription,
    gate: &Gate,
    provider_call_id: &str,
) -> Vec<RecoverableChatUpdate> {
    let mut seen = Vec::new();
    loop {
        let update = next_update(updates).await;
        let started = matches!(
            &update,
            RecoverableChatUpdate::Event { event, .. }
                if matches!(
                    &event.payload,
                    SessionObservationEventPayload::TurnActivity(TurnActivity {
                        event: TurnEvent::ToolCallStarted { provider_call_id: Some(id), .. },
                        ..
                    }) if id == provider_call_id
                )
        );
        seen.push(update);
        if started {
            break;
        }
    }
    gate.open();
    seen.extend(through_next_commit(updates).await);
    seen
}

/// The row ids of `view`'s transcript, in order.
fn row_ids(view: &lash_core::SessionReadView) -> Vec<lash_core_store::transcript::RowId> {
    view.transcript()
        .expect("the transcript projects")
        .into_records()
        .into_iter()
        .map(|row| row.row_id)
        .collect()
}

/// A host sink: every activity it was handed, in order.
#[derive(Default)]
struct Recorded {
    activities: Mutex<Vec<TurnActivity>>,
    count: tokio::sync::watch::Sender<usize>,
}

impl Recorded {
    fn activities(&self) -> Vec<TurnActivity> {
        self.activities.lock_recover().clone()
    }

    /// Wait until the activities handed so far satisfy `ready`.
    async fn until(&self, ready: impl Fn(&[TurnActivity]) -> bool) {
        let mut count = self.count.subscribe();
        loop {
            if ready(&self.activities()) {
                return;
            }
            count.changed().await.expect("the sink outlives its wait");
        }
    }
}

#[async_trait::async_trait]
impl lash_core::facade_support::TurnActivitySink for Recorded {
    async fn emit(&self, activity: TurnActivity) {
        self.activities.lock_recover().push(activity);
        self.count.send_modify(|count| *count += 1);
    }
}

/// The lash id of the call the model issued as `provider_call_id`, from
/// its `ToolCallStarted`.
fn started(activities: &[TurnActivity], provider_call_id: &str) -> Vec<lash_core::ToolCallId> {
    activities
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::ToolCallStarted {
                call_id,
                provider_call_id: Some(provider),
                ..
            } if provider == provider_call_id => Some(call_id.clone()),
            _ => None,
        })
        .collect()
}

/// The position of every activity whose event `matches`.
fn positions(activities: &[TurnActivity], matches: impl Fn(&TurnEvent) -> bool) -> Vec<usize> {
    activities
        .iter()
        .enumerate()
        .filter(|(_, activity)| matches(&activity.event))
        .map(|(position, _)| position)
        .collect()
}

/// A durable turn's sink is handed a native call's `ToolCallStarted` live,
/// while the call's body holds and before its round's outcome can commit,
/// and its `ToolIntentOutcome` and `ToolCallCompleted` once it settles:
/// each once, in that order, on an uncut run.
async fn a_durable_turn_streams_a_native_call_s_activity_to_its_sink(tier: Tier) {
    const NAME: &str = "activity law: run the gated call";
    const PROVIDER_CALL: &str = "activity-call";
    let gate = Gate::new(false);
    let Some(world) = world(tier, &gate, false).await else {
        return;
    };
    world.script(
        NAME,
        vec![served::response(vec![served::call(
            PROVIDER_CALL,
            GATED,
            serde_json::json!({ "label": "gated", "start": true }),
        )])],
    );
    let session = world.session("activity-live", served::spec(8)).await;
    let sink = Arc::new(Recorded::default());
    let handle = session
        .send(lash::TurnInput::text(NAME))
        .await
        .expect("the input is accepted");
    let follower = {
        let sink = Arc::clone(&sink);
        tokio::spawn(async move { handle.outcome_into(sink.as_ref()).await })
    };

    // The body holds on the closed gate: the call has not settled, so its
    // round has no outcome to commit.
    tokio::time::timeout(
        WATCHDOG,
        sink.until(|activities| !started(activities, PROVIDER_CALL).is_empty()),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "the sink was handed no ToolCallStarted while the call ran: {:#?}",
            sink.activities()
        )
    });
    assert!(
        !sink.activities().iter().any(|activity| matches!(
            activity.event,
            TurnEvent::ToolCallCompleted { .. } | TurnEvent::ToolIntentOutcome { .. }
        )),
        "nothing settled the held call: {:#?}",
        sink.activities()
    );

    gate.open();
    let outcome = tokio::time::timeout(WATCHDOG, follower)
        .await
        .expect("deadlock watchdog: the turn never settled")
        .expect("the follower ran")
        .expect("the turn answers");
    let lash::SendOutcome::Settled { output, .. } = outcome else {
        panic!("the turn settles: {outcome:?}");
    };
    served::assert_answered("the gated turn", &output);
    let activities = sink.activities();
    let ids = started(&activities, PROVIDER_CALL);
    assert_eq!(ids.len(), 1, "one start: {activities:#?}");
    let call = &ids[0];
    let start = positions(
        &activities,
        |event| matches!(event, TurnEvent::ToolCallStarted { call_id, .. } if call_id == call),
    );
    let intents = positions(&activities, |event| {
        matches!(
            event,
            TurnEvent::ToolIntentOutcome {
                call_id,
                outcome: lash_core::ToolIntentExecutionOutcome::Executed {
                    realized: lash_core::ToolIntentRealized::StartProcess(_),
                    ..
                },
            } if call_id == call
        )
    });
    let completed = positions(
        &activities,
        |event| matches!(event, TurnEvent::ToolCallCompleted { call_id, .. } if call_id == call),
    );
    assert!(
        intents.len() == 1 && completed.len() == 1,
        "one executed start receipt and one completion: {activities:#?}"
    );
    assert!(
        start[0] < intents[0] && intents[0] < completed[0],
        "the start, then the receipt, then the completion: {activities:#?}"
    );
    world.shutdown().await;
}

/// A plugin's presentation step presents a native round call that answers
/// at once and one that parks and is resolved out of band: the turn records
/// the presented return, and the model is shown it.
async fn a_presentation_step_presents_every_native_round_call(tier: Tier) {
    const NAME: &str = "activity law: present both calls";
    let gate = Gate::new(true);
    let Some(world) = world(tier, &gate, true).await else {
        return;
    };
    let output = world
        .run(
            NAME,
            served::spec(8),
            vec![served::response(vec![
                served::call(
                    "presented-at-once",
                    GATED,
                    serde_json::json!({ "label": "at-once" }),
                ),
                served::call(
                    "presented-after-park",
                    DEFERRED,
                    serde_json::json!({ "label": "after-park" }),
                ),
            ])],
        )
        .await;
    served::assert_answered("the presented turn", &output);
    let results = served::results(&output);
    for provider_call_id in ["presented-at-once", "presented-after-park"] {
        let result = results
            .iter()
            .find(|result| result.provider_call_id == provider_call_id)
            .unwrap_or_else(|| panic!("the turn recorded {provider_call_id}: {results:#?}"));
        assert!(
            result.content.contains(PRESENTED),
            "the recorded result of {provider_call_id} is the step's presentation: {result:#?}"
        );
    }
    let asked = world.requests(NAME);
    let shown = asked.get(1).expect("the model was asked after the calls");
    assert_eq!(
        shown.matches(PRESENTED).count(),
        2,
        "the model was shown both presentations: {shown}"
    );
    world.shutdown().await;
}

/// Through the recoverable chat, a turn's provisional tool activity is
/// followed by exactly one `TerminalReplacement`, whose rows are the rows
/// the turn's commit added; the next turn's commit is the next one.
async fn a_turn_s_activity_is_settled_by_one_terminal_replacement_of_its_rows(tier: Tier) {
    const NAME: &str = "activity law: settle the call";
    const SESSION: &str = "activity-settled";
    let gate = Gate::new(false);
    let Some(world) = world(tier, &gate, false).await else {
        return;
    };
    world.script(
        NAME,
        vec![served::response(vec![served::call(
            "settled-call",
            GATED,
            serde_json::json!({ "label": "settled" }),
        )])],
    );
    let session = world.session(SESSION, served::spec(8)).await;
    let observed = world
        .core
        .session(lash::SessionId::try_from(SESSION.to_owned()).expect("a session id"))
        .open()
        .await
        .expect("the session opens for observation");
    let snapshot = observed
        .observe()
        .recoverable_chat_snapshot()
        .await
        .expect("the snapshot reads the durable head");
    let mut updates = observed
        .observe()
        .subscribe_recoverable_chat(snapshot.cursor.clone());

    let (output, first) = tokio::join!(
        world.send(&session, NAME),
        through_the_held_call_s_commit(&mut updates, &gate, "settled-call"),
    );
    served::assert_answered("the settled turn", &output);
    let head = session
        .read()
        .await
        .expect("the head reads")
        .expect("the session has a head");
    served::assert_answered(
        "the next turn",
        &world.send(&session, "activity law: the next turn").await,
    );
    let second = through_next_commit(&mut updates).await;

    assert!(
        first
            .iter()
            .chain(&second)
            .all(|update| !matches!(update, RecoverableChatUpdate::ReplayGap { .. })),
        "a subscriber that keeps up meets no gap: {first:#?} {second:#?}"
    );
    assert!(
        first.iter().any(|update| matches!(
            update,
            RecoverableChatUpdate::Event { event, .. }
                if matches!(
                    &event.payload,
                    SessionObservationEventPayload::TurnActivity(TurnActivity {
                        event: TurnEvent::ToolCallStarted { provider_call_id: Some(id), .. },
                        ..
                    }) if id == "settled-call"
                )
        )),
        "the call's start streamed before the commit: {first:#?}"
    );
    let Some(RecoverableChatUpdate::TerminalReplacement { event, .. }) = first.last() else {
        unreachable!("through_next_commit ends at a replacement");
    };
    let SessionObservationEventPayload::Committed {
        base_revision,
        rows,
    } = &event.payload
    else {
        panic!("a replacement is a commit: {event:#?}");
    };
    let held = row_ids(&snapshot.read_view);
    let added = row_ids(&head)
        .into_iter()
        .filter(|row| !held.contains(row))
        .collect::<Vec<_>>();
    assert!(!added.is_empty(), "the turn's commit added rows");
    assert_eq!(
        rows.iter()
            .map(|row| row.row_id.clone())
            .collect::<Vec<_>>(),
        added,
        "the replacement carries the rows the commit added"
    );
    let Some(RecoverableChatUpdate::TerminalReplacement { event: next, .. }) = second.last() else {
        unreachable!("through_next_commit ends at a replacement");
    };
    let SessionObservationEventPayload::Committed {
        base_revision: next_base,
        ..
    } = &next.payload
    else {
        panic!("a replacement is a commit: {next:#?}");
    };
    assert!(
        *base_revision < event.revision()
            && *next_base == event.revision()
            && next.revision() > event.revision(),
        "each commit's replacement arrives once, in revision order: {first:#?} {second:#?}"
    );
    world.shutdown().await;
}

/// A commit one node makes reaches a subscriber attached through another
/// core that shares the deployment's live replay store and serves no node.
async fn a_commit_on_one_node_reaches_a_subscriber_attached_through_another(tier: Tier) {
    const NAME: &str = "activity law: commit on the serving node";
    const SESSION: &str = "activity-two-nodes";
    let live: Arc<dyn LiveReplayStore> =
        Arc::new(lash_core::facade_support::InMemoryLiveReplayStore::default());
    let gate = Gate::new(false);
    let Some(world) = world_with(tier, &gate, false, Some(Arc::clone(&live))).await else {
        return;
    };
    world.script(
        NAME,
        vec![served::response(vec![served::call(
            "two-node-call",
            GATED,
            serde_json::json!({ "label": "two-nodes" }),
        )])],
    );
    let attached = lash::LashCore::standard_builder(world.backend.clone())
        .serve_sessions(false)
        .live_replay_store(live)
        .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .serve_test_llm_profile(
            served::model(Arc::clone(&world.scripts)),
            served::metadata(),
        )
        .tools(Arc::new(ActivityTools {
            gate: gate.clone(),
            backend: world.backend.clone(),
        }))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "activity-attached-deployment",
            "activity-attached-boot",
        ))
        .expect("the attached core builds");
    let session_id = lash::SessionId::try_from(SESSION.to_owned()).expect("a session id");
    let session = attached
        .session(session_id.clone())
        .create(lash::SessionCreation::root(served::spec(8)))
        .await
        .expect("the session is created");
    let observed = attached
        .session(session_id)
        .open()
        .await
        .expect("the session opens for observation");
    let snapshot = observed
        .observe()
        .recoverable_chat_snapshot()
        .await
        .expect("the snapshot reads the durable head");
    let mut updates = observed
        .observe()
        .subscribe_recoverable_chat(snapshot.cursor.clone());

    let (output, seen) = tokio::join!(
        async {
            tokio::time::timeout(WATCHDOG, session.send(lash::TurnInput::text(NAME)).output())
                .await
                .expect("deadlock watchdog: the turn never settled")
                .expect("the turn answers")
        },
        through_the_held_call_s_commit(&mut updates, &gate, "two-node-call"),
    );
    served::assert_answered("the two-node turn", &output);
    let head = session
        .read()
        .await
        .expect("the head reads")
        .expect("the session has a head");
    let Some(RecoverableChatUpdate::TerminalReplacement { event, .. }) = seen.last() else {
        unreachable!("through_next_commit ends at a replacement");
    };
    let SessionObservationEventPayload::Committed { rows, .. } = &event.payload else {
        panic!("a replacement is a commit: {event:#?}");
    };
    let held = row_ids(&snapshot.read_view);
    assert_eq!(
        rows.iter()
            .map(|row| row.row_id.clone())
            .collect::<Vec<_>>(),
        row_ids(&head)
            .into_iter()
            .filter(|row| !held.contains(row))
            .collect::<Vec<_>>(),
        "the serving node's commit reached the attached subscriber with its rows: {seen:#?}"
    );
    attached
        .shutdown()
        .await
        .expect("the attached core shuts down");
    world.shutdown().await;
}

tiered_laws!(
    a_durable_turn_streams_a_native_call_s_activity_to_its_sink,
    a_presentation_step_presents_every_native_round_call,
    a_turn_s_activity_is_settled_by_one_terminal_replacement_of_its_rows,
    a_commit_on_one_node_reaches_a_subscriber_attached_through_another,
);
