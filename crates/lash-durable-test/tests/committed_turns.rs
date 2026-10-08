//! A host reads a session's committed turns oldest-first after a cursor,
//! without gaps or repeats, through the facade (FIG-5297, ADR 0129).
//!
//! Each law runs a lash core serving its own node over one tier's database
//! and reads `DurableSession::committed_turns` the way a post-processing host
//! does: from its saved cursor, persisting the page's `next`.
#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash::persistence::CommittedTurnCursor;
use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash::transcript::{CommittedTurn, TranscriptRowKind};
use lash_core::store::TurnCommitOutcome;
use lash_core::{ToolCall, ToolControl, ToolOutcome};
use served::{Tier, WATCHDOG, World};

/// A page small enough that every read pages.
const PAGE: std::num::NonZeroU32 = std::num::NonZeroU32::new(2).unwrap();

/// How often a law polls for a turn that has not committed yet.
const POLL_EVERY: Duration = Duration::from_millis(20);

/// One page after `after`: its turns and its `next`.
async fn read(
    session: &lash::DurableSession,
    after: Option<&CommittedTurnCursor>,
) -> (Vec<CommittedTurn>, CommittedTurnCursor) {
    let page = session
        .committed_turns(after, PAGE)
        .await
        .expect("read the session's committed turns");
    (page.turns, page.next)
}

/// Follow the session from `after` until `count` turns have committed, then
/// check that nothing follows them.
async fn follow(
    session: &lash::DurableSession,
    after: Option<&CommittedTurnCursor>,
    count: usize,
) -> (Vec<CommittedTurn>, CommittedTurnCursor) {
    tokio::time::timeout(WATCHDOG, async {
        let mut cursor = after.cloned();
        let mut turns = Vec::new();
        while turns.len() < count {
            let (page, next) = read(session, cursor.as_ref()).await;
            if page.is_empty() {
                tokio::time::sleep(POLL_EVERY).await;
            }
            turns.extend(page);
            cursor = Some(next);
        }
        let cursor = cursor.expect("a read returns a cursor");
        let (more, next) = read(session, Some(&cursor)).await;
        assert!(more.is_empty(), "no turn follows the {count} expected");
        assert_eq!(next, cursor, "an empty page keeps the cursor");
        (turns, cursor)
    })
    .await
    .expect("deadlock watchdog: the expected turns never committed")
}

/// Whether `row` is an accepted input's user row.
fn is_input_row(row: &lash::transcript::TranscriptRowRecord) -> bool {
    row.suppressed.is_none()
        && row.kind == TranscriptRowKind::User
        && row.provenance.input_id.is_some()
}

/// The texts of a turn's input rows, in order.
fn user_texts(turn: &CommittedTurn) -> Vec<String> {
    turn.rows
        .iter()
        .filter(|row| is_input_row(row))
        .map(|row| row.content.text.clone())
        .collect()
}

/// Every committed input row of `session`, turn by turn, is exactly the
/// transcript's, in order, and names the turn that committed it.
async fn assert_turns_hold_every_user_row(session: &lash::DurableSession, turns: &[CommittedTurn]) {
    let mut read = Vec::new();
    for turn in turns {
        for row in &turn.rows {
            if is_input_row(row) {
                assert_eq!(
                    row.provenance.turn_id.as_ref(),
                    Some(&turn.turn_id),
                    "a user row names the turn that committed it"
                );
                read.push(row.clone());
            }
        }
    }
    let transcript = session.transcript().await.expect("read the transcript");
    let expected: Vec<_> = transcript
        .visible()
        .filter(|row| is_input_row(row))
        .cloned()
        .collect();
    assert_eq!(
        read, expected,
        "the committed turns hold every user row of the session once"
    );
}

const SWITCH_TOOL: &str = "switch_frame";
const SWITCH: &str = "switch frames, then carry on";
const TASK: &str = "summarise where the work stands";

/// `switch_frame`'s body: it switches the agent frame with a task.
struct SwitchFrame;

#[async_trait::async_trait]
impl StaticToolExecute for SwitchFrame {
    async fn execute(&self, _call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        ToolOutcome::ok(serde_json::json!({ "ok": true }))
            .with_control(ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material("committed-turns-law")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some(TASK.to_owned()),
            })
            .into()
    }
}

fn switch_frame() -> Arc<dyn lash_core::ToolProvider> {
    let definition = lash_core::ToolDefinition::raw(
        SWITCH_TOOL,
        SWITCH_TOOL,
        "Switches the agent frame and hands the new frame a task.",
        serde_json::json!({ "type": "object", "additionalProperties": false, "properties": {} }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("switch_frame's schemas")
    .with_execution(std::time::Duration::from_secs(120));
    Arc::new(StaticToolProvider::new(vec![definition], SwitchFrame))
}

/// A cursor read before a compaction reads the next turn after it, and one
/// read before an agent frame switch reads the switching turn and then its
/// follow-on: compaction commits no turn, and a frame switch changes no
/// position.
async fn a_cursor_reads_on_across_compaction_and_a_frame_switch(tier: Tier) {
    let Some(world) = World::new(tier, |backend| {
        lash::LashCore::standard_builder(backend.clone())
            .tools(switch_frame())
            .plugin(Arc::new(
                lash_plugin_standard_compaction::StandardCompactionPluginFactory::default(),
            ))
    })
    .await
    else {
        return;
    };
    world.script(
        SWITCH,
        vec![served::response(vec![served::call(
            "switch-call",
            SWITCH_TOOL,
            serde_json::json!({}),
        )])],
    );
    let session = world
        .session("committed-turns-frames", served::spec(8))
        .await;
    for text in ["first turn", "second turn"] {
        served::assert_answered(text, &world.send(&session, text).await);
    }
    let (turns, cursor) = follow(&session, None, 2).await;
    assert_eq!(
        turns.iter().map(user_texts).collect::<Vec<_>>(),
        [["first turn"], ["second turn"]]
    );

    let live = world
        .core
        .session(session.session_id().clone())
        .open()
        .await
        .expect("open the session");
    assert!(
        live.admin()
            .state()
            .compact_context(None)
            .await
            .expect("the compaction settles"),
        "the compaction opened a frame"
    );
    drop(live);
    let (none, after_compaction) = read(&session, Some(&cursor)).await;
    assert!(none.is_empty(), "a compaction commits no turn");
    assert_eq!(after_compaction, cursor);
    served::assert_answered(
        "after compaction",
        &world.send(&session, "after compaction").await,
    );
    let (turns, cursor) = follow(&session, Some(&cursor), 1).await;
    assert_eq!(user_texts(&turns[0]), ["after compaction"]);
    assert_eq!(turns[0].outcome, TurnCommitOutcome::Completed);

    world.send(&session, SWITCH).await;
    let (turns, _) = follow(&session, Some(&cursor), 2).await;
    assert_eq!(turns[0].outcome, TurnCommitOutcome::FrameSwitch);
    assert_eq!(user_texts(&turns[0]), [SWITCH]);
    assert_eq!(
        turns[1].outcome,
        TurnCommitOutcome::Completed,
        "the follow-on answers on the new frame"
    );
    let (all, _) = follow(&session, None, 5).await;
    assert_turns_hold_every_user_row(&session, &all).await;
    world.shutdown().await;
}

fn turn(id: &str) -> lash::TurnId {
    lash::TurnId::try_from(id.to_owned()).expect("a host turn id")
}

async fn send(session: &lash::DurableSession, run: &str, text: &str) {
    let output = tokio::time::timeout(
        WATCHDOG,
        session
            .send(lash::TurnInput::text(text))
            .id(turn(run))
            .output(),
    )
    .await
    .expect("deadlock watchdog: the turn never settled")
    .expect("the turn answers");
    served::assert_answered(text, &output);
}

/// A fork's first read serves only the turns the fork commits, never the
/// history it inherited; its source reads only its own.
async fn a_forks_first_read_serves_only_its_own_turns(tier: Tier) {
    let Some(world) = World::new(tier, |backend| {
        lash::LashCore::standard_builder(backend.clone())
    })
    .await
    else {
        return;
    };
    let source = world
        .session("committed-turns-source", served::spec(8))
        .await;
    send(&source, "source-one", "the source's first turn").await;
    send(&source, "source-two", "the source's second turn").await;
    let fork_id = lash::SessionId::try_from("committed-turns-fork".to_owned()).unwrap();
    world
        .core
        .fork_at(
            source.session_id(),
            lash_core::Target::Turn(turn("source-two")),
            lash::ForkRequest {
                session_id: fork_id.clone(),
                relation: lash_core::SessionRelation::Fork {
                    source_session_id: source.session_id().clone(),
                    source_node_id: None,
                },
                observed_processes: Vec::new(),
            },
        )
        .await
        .expect("fork the source after its second turn");
    let fork = world
        .core
        .session(fork_id)
        .durable()
        .await
        .expect("the fork opens");
    let (inherited, start) = read(&fork, None).await;
    assert!(inherited.is_empty(), "the fork serves no inherited turn");
    send(&fork, "fork-one", "the fork's own turn").await;
    let (turns, _) = follow(&fork, Some(&start), 1).await;
    assert_eq!(turns[0].turn_id, turn("fork-one"));
    assert_eq!(user_texts(&turns[0]), ["the fork's own turn"]);
    let (from_start, _) = follow(&fork, None, 1).await;
    assert_eq!(from_start, turns);

    let (source_turns, _) = follow(&source, None, 2).await;
    assert_eq!(
        source_turns
            .iter()
            .map(|turn| turn.turn_id.clone())
            .collect::<Vec<_>>(),
        [turn("source-one"), turn("source-two")],
        "the source reads only its own turns"
    );
    let foreign = source
        .committed_turns(Some(&start), PAGE)
        .await
        .expect_err("the fork's cursor cannot page its source");
    assert!(
        matches!(
            foreign,
            lash::EmbedError::Store(lash_core::StoreError::CursorForeignSession { .. })
        ),
        "the refusal is typed: {foreign:?}"
    );
    world.shutdown().await;
}

/// A cursor a host saved, serialized, before its process stopped reads the
/// next turn after a new process over the same database commits it.
async fn a_cursor_survives_a_process_restart(tier: Tier) {
    let Some((stores, keep)) = served::stores(tier).await else {
        return;
    };
    let scripts = Arc::new(served::Scripts::default());
    let build = |backend: &lash::Backend, boot: &str| {
        lash::LashCore::standard_builder(backend.clone())
            .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .serve_test_llm_profile(served::model(Arc::clone(&scripts)), served::metadata())
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                "committed-turns-restart",
                boot,
            ))
            .expect("the core builds")
    };
    let session_id = lash::SessionId::try_from("committed-turns-restart".to_owned()).unwrap();
    let saved = {
        let backend = served::backend(Arc::clone(&stores));
        let core = build(&backend, "first-boot");
        let session = core
            .session(session_id.clone())
            .create(lash::SessionCreation::root(served::spec(8)))
            .await
            .expect("create the session");
        send(&session, "before-restart", "before the restart").await;
        let (turns, cursor) = follow(&session, None, 1).await;
        assert_eq!(turns[0].turn_id, turn("before-restart"));
        core.shutdown().await.expect("stop the first process");
        serde_json::to_string(&cursor).expect("the cursor serializes")
    };
    let backend = served::backend(stores);
    let core = build(&backend, "second-boot");
    let session = core
        .session(session_id)
        .durable()
        .await
        .expect("the session opens after the restart");
    send(&session, "after-restart", "after the restart").await;
    let cursor: CommittedTurnCursor =
        serde_json::from_str(&saved).expect("the saved cursor deserializes");
    let (turns, _) = follow(&session, Some(&cursor), 1).await;
    assert_eq!(turns[0].turn_id, turn("after-restart"));
    assert_eq!(user_texts(&turns[0]), ["after the restart"]);
    core.shutdown().await.expect("stop the second process");
    drop(keep);
}

/// A model that holds its first request until the law opens its gate and
/// answers every request in prose.
fn gated_model(
    started: Arc<tokio::sync::Notify>,
    gate: Arc<tokio::sync::Semaphore>,
) -> lash_core::facade_support::ProviderHandle {
    let calls = Arc::new(AtomicUsize::new(0));
    lash_core::testing::TestProvider::builder()
        .kind("committed-turns-gated-model")
        .complete(move |request: lash_core::llm::types::LlmRequest| {
            let started = Arc::clone(&started);
            let gate = Arc::clone(&gate);
            let calls = Arc::clone(&calls);
            async move {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    started.notify_one();
                    gate.acquire()
                        .await
                        .expect("the law opens the model gate")
                        .forget();
                }
                Ok(served::text(&request, "done"))
            }
        })
        .build()
        .into_handle()
}

/// FIG-5288, FIG-5293: a steering input sent while a turn runs, and a queued
/// one, each commit their user row inside a committed turn; the turns hold
/// every user row of the session, each naming its turn.
async fn a_steered_turns_rows_include_every_user_row(tier: Tier) {
    let started = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let Some(world) = World::with_model(
        tier,
        Vec::new(),
        gated_model(Arc::clone(&started), Arc::clone(&gate)),
        |backend| lash::LashCore::standard_builder(backend.clone()),
    )
    .await
    else {
        return;
    };
    let session = world
        .session("committed-turns-steering", served::spec(8))
        .await;
    let live = world
        .core
        .session(session.session_id().clone())
        .open()
        .await
        .expect("open the session");
    tokio::time::timeout(WATCHDOG, async {
        let opening = live
            .send(lash::TurnInput::text("opening input"))
            .id(turn("steered-opening"))
            .await
            .expect("accept the opening input");
        started.notified().await;
        let queued = session
            .send(lash::TurnInput::text("queued input"))
            .id(turn("steered-queued"))
            .await
            .expect("accept the queued input");
        let steering = live
            .send(lash::TurnInput::text("steering input"))
            .id(turn("steered-steering"))
            .ingress(lash::persistence::TurnInputIngress::active_turn(
                turn("steered-opening"),
                Default::default(),
            ))
            .await
            .expect("accept the steering input");
        gate.add_permits(1);
        for handle in [opening, queued, steering] {
            served::assert_answered("steering", &handle.output().await.unwrap());
        }
    })
    .await
    .expect("deadlock watchdog: the steered inputs never answered");
    drop(live);
    let turns = all_turns(&session).await;
    assert_eq!(turns[0].turn_id, turn("steered-opening"));
    let texts: Vec<String> = turns.iter().flat_map(user_texts).collect();
    assert_eq!(
        texts,
        ["opening input", "queued input", "steering input"],
        "every input's row is in a committed turn"
    );
    assert_turns_hold_every_user_row(&session, &turns).await;
    world.shutdown().await;
}

/// Every turn the session has committed, read from its start in pages.
async fn all_turns(session: &lash::DurableSession) -> Vec<CommittedTurn> {
    let mut cursor = None;
    let mut turns = Vec::new();
    loop {
        let (page, next) = read(session, cursor.as_ref()).await;
        if page.is_empty() {
            return turns;
        }
        turns.extend(page);
        cursor = Some(next);
    }
}

const BUSY: &str = "hold the session while the queue fills";
const COALESCED: [&str; 3] = ["coalesced one", "coalesced two", "coalesced three"];

/// The inputs a coalescing drain admits as one run commit as one turn whose
/// rows hold every one of their user rows, in queue order.
async fn a_coalesced_turns_rows_include_every_user_row(tier: Tier) {
    let started = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let Some(world) = World::with_batching(
        tier,
        lash::QueuedWorkBatchingConfig::new(1).with_drain_mode(lash::DrainMode::All),
        gated_model(Arc::clone(&started), Arc::clone(&gate)),
        |backend| lash::LashCore::standard_builder(backend.clone()),
    )
    .await
    else {
        return;
    };
    let session = world
        .session("committed-turns-coalesced", served::spec(8))
        .await;
    tokio::time::timeout(WATCHDOG, async {
        let busy = session
            .send(lash::TurnInput::text(BUSY))
            .id(turn("coalesced-busy"))
            .await
            .expect("send the busy input");
        started.notified().await;
        let mut queued = Vec::new();
        for (index, text) in COALESCED.iter().enumerate() {
            queued.push(
                session
                    .send(lash::TurnInput::text(*text))
                    .id(turn(&format!("coalesced-{index}")))
                    .await
                    .expect("queue an input"),
            );
        }
        gate.add_permits(1);
        busy.output().await.expect("the busy run answers");
        for handle in queued {
            served::assert_answered("coalesced", &handle.output().await.unwrap());
        }
    })
    .await
    .expect("deadlock watchdog: the coalesced run never settled");
    let (turns, _) = follow(&session, None, 2).await;
    assert_eq!(turns[0].turn_id, turn("coalesced-busy"));
    assert_eq!(user_texts(&turns[0]), [BUSY]);
    assert_eq!(turns[1].turn_id, turn("coalesced-0"));
    assert_eq!(
        user_texts(&turns[1]),
        COALESCED,
        "the coalesced turn holds each input's row, in queue order"
    );
    assert_turns_hold_every_user_row(&session, &turns).await;
    world.shutdown().await;
}

tiered_laws!(
    a_cursor_reads_on_across_compaction_and_a_frame_switch,
    a_forks_first_read_serves_only_its_own_turns,
    a_cursor_survives_a_process_restart,
    a_steered_turns_rows_include_every_user_row,
    a_coalesced_turns_rows_include_every_user_row,
);
