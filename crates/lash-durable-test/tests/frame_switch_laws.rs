//! An agent frame switch on a lash core serving its own node: what the
//! switch leaves behind it, and what runs after it (ADR 0101 §3, ADR 0112
//! §5).
//!
//! The scripted model calls the host tool `switch_frame`, whose control
//! switches the agent frame with a task. The turn's commit publishes the new
//! frame and mails the session its follow-on in the same transaction.
#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use lash::persistence::{SessionHeadRef, WindowSelector};
use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash::transcript::CommittedTurn;
use lash_core::store::TurnCommitOutcome;
use lash_core::{ToolCall, ToolControl, ToolOutcome};
use served::{Tier, WATCHDOG, World};

const SWITCH_TOOL: &str = "switch_frame";
const FIRST: &str = "first turn";
const SWITCH: &str = "switch frames, then carry on";
const TASK: &str = "summarise where the work stands";
const QUEUED: &str = "a request sent while the switch runs";

/// How often a law polls for a turn that has not committed yet.
const POLL_EVERY: Duration = Duration::from_millis(20);

/// `switch_frame`'s body: it switches the agent frame with [`TASK`], after
/// sending the session [`QUEUED`] when the law gives it a core to send
/// through.
struct SwitchFrame(Arc<OnceLock<lash::LashCore>>, &'static str);

#[async_trait::async_trait]
impl StaticToolExecute for SwitchFrame {
    async fn execute(&self, _call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if let Some(core) = self.0.get() {
            let session = core
                .session(lash::SessionId::try_from(self.1.to_owned()).expect("a session id"))
                .durable()
                .await
                .expect("the session's durable handle");
            session
                .send(lash::TurnInput::text(QUEUED))
                .id(lash::TurnId::try_from("queued-during-switch".to_owned()).expect("a host id"))
                .await
                .expect("the session queues the input");
        }
        ToolOutcome::ok(serde_json::json!({ "ok": true }))
            .with_control(ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material(self.1)
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some(TASK.to_owned()),
            })
            .into()
    }
}

/// The core of `session`, whose `switch_frame` sends through `sender` once
/// it is set.
async fn world(
    tier: Tier,
    session: &'static str,
    sender: Arc<OnceLock<lash::LashCore>>,
) -> Option<World> {
    let definition = lash_core::ToolDefinition::raw(
        SWITCH_TOOL,
        SWITCH_TOOL,
        "Switches the agent frame and hands the new frame a task.",
        serde_json::json!({ "type": "object", "additionalProperties": false, "properties": {} }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("switch_frame's schemas");
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(StaticToolProvider::new(
        vec![definition],
        SwitchFrame(sender, session),
    ));
    let world = World::new(tier, |backend| {
        lash::LashCore::standard_builder(backend.clone()).tools(tools)
    })
    .await?;
    world.script(
        SWITCH,
        vec![served::response(vec![served::call(
            "switch-call",
            SWITCH_TOOL,
            serde_json::json!({}),
        )])],
    );
    world.script(TASK, Vec::new());
    world.script(QUEUED, Vec::new());
    Some(world)
}

/// The session's first `count` committed turns, once they have committed.
async fn committed(session: &lash::DurableSession, count: usize) -> Vec<CommittedTurn> {
    let page = std::num::NonZeroU32::new(16).expect("a page size");
    tokio::time::timeout(WATCHDOG, async {
        loop {
            let read = session
                .committed_turns(None, page)
                .await
                .expect("read the session's committed turns");
            if read.turns.len() >= count {
                return read.turns;
            }
            tokio::time::sleep(POLL_EVERY).await;
        }
    })
    .await
    .expect("deadlock watchdog: the expected turns never committed")
}

/// The admitted window of a turn is its admission frame after the turn's
/// own commit switched frames (ADR 0112 §5, §14.4): a read pinned at the
/// head the switching turn was admitted on stays anchored at that frame,
/// under its config and with its rows, while the live head reads the new
/// frame.
async fn the_admitted_window_of_a_frame_switching_turn_is_its_admission_frame(tier: Tier) {
    const SESSION: &str = "frame-switch-admitted-window";
    let Some(world) = world(tier, SESSION, Arc::default()).await else {
        return;
    };
    let session = world.session(SESSION, served::spec(8)).await;
    served::assert_answered(FIRST, &world.send(&session, FIRST).await);
    let store = world.backend.session_store_factory();
    let id = lash::SessionId::try_from(SESSION.to_owned()).expect("a session id");
    let admitted = store
        .load_session_window(&id, WindowSelector::Current)
        .await
        .expect("read the head")
        .expect("the session has a head");
    let first_frame = admitted
        .current_frame_node_id
        .clone()
        .expect("the first turn opened a frame");
    let base = SessionHeadRef {
        generation: 0,
        revision: admitted.head_revision,
        leaf: admitted.window.leaf_node_id.clone(),
        checkpoint: admitted.checkpoint_ref.clone(),
    };

    world.send(&session, SWITCH).await;
    let turns = committed(&session, 3).await;
    assert_eq!(turns[1].outcome, TurnCommitOutcome::FrameSwitch);
    let live = store
        .load_session_window(&id, WindowSelector::Current)
        .await
        .expect("read the live head")
        .expect("the session has a head");
    assert_ne!(
        live.current_frame_node_id.as_ref(),
        Some(&first_frame),
        "the switching turn's own commit moved the head to a new frame"
    );

    let read = store
        .load_session_window(&id, WindowSelector::Admitted(base))
        .await
        .expect("read the admitted window")
        .expect("the admission base is retained");
    let anchor = read
        .window
        .anchor()
        .expect("an admitted window is anchored");
    assert_eq!(
        anchor.frame_node_id, first_frame,
        "anchored at the first frame"
    );
    assert_eq!(read.config, admitted.config, "the first frame's config");
    assert_eq!(
        serde_json::to_value(&read.window.nodes).expect("encode the admitted window"),
        serde_json::to_value(&admitted.window.nodes).expect("encode the admission window"),
        "the admitted window holds the rows the turn was admitted on"
    );
    world.shutdown().await;
}

tiered_laws!(the_admitted_window_of_a_frame_switching_turn_is_its_admission_frame,);
