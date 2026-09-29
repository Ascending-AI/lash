//! Identity across the boundaries a session's history crosses: code cells
//! replayed over their journal, an agent frame switch (a follow-on frame, as
//! `continue_as` opens) and a compaction. A call replayed across the boundary keeps its identity; a
//! fresh call on the other side of it is a different call, even when the
//! provider hands it the same call id.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::laws::{
    PanicBeforeTurnCommit, assert_distinct_keys, assert_one_identity, crash_while_held, only,
};
use super::{
    PROBE, ProbeArgs, ToolCallIdentityTier, World, assert_finished, calls, cell, follow_on_task,
    text,
};

/// Panics as the first committed turn's delivery begins: the switch commit is
/// durable and the root has not ended.
struct PanicAfterSwitchCommit;

impl lash_core::runtime::RuntimeTurnPhaseProbe for PanicAfterSwitchCommit {
    fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::PostCommitDelivery {
            panic!("injected crash after the switched turn's commit");
        }
    }

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn begin_named(&self, _phase: &str) {}
}

type DriveReport = tokio::sync::mpsc::UnboundedSender<
    Result<crate::facade_support::QueuedTurnDrain<crate::AssembledTurn>, crate::RuntimeError>,
>;

/// One attempt at the queued root: the crashing one dies after the switch
/// commit, the redrive reports how its drive ended.
fn root_attempt(world: &World, report: Option<DriveReport>) -> crate::ConformanceTurnAttempt {
    let world = world.clone();
    Arc::new(move |scope| {
        let world = world.clone();
        let report = report.clone();
        Box::pin(async move {
            let probe = report
                .is_none()
                .then(|| Arc::new(PanicAfterSwitchCommit) as Arc<_>);
            let mut runtime = world.runtime(probe).await;
            let drive = Box::pin(runtime.drive_next_queued_root(crate::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                scope,
            )))
            .await;
            let Some(report) = report else {
                panic!(
                    "the crash probe did not fire after the switch commit: {:?}",
                    drive.map(crate::facade_support::QueuedTurnDrain::ran)
                );
            };
            let end = crate::ConformanceTurnEnd::of(&drive);
            let _ = report.send(drive);
            end
        })
    })
}

/// A root whose first frame calls the probe as `call_0` and switches to a
/// follow-on frame, where the model calls the probe as `call_0` again. The
/// root dies after the switch commit and is redriven. The first frame's call
/// is read back and never runs again; the follow-on's call is a fresh call
/// whose identity differs from the first frame's.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn frames_keep_identity_and_distinguish_fresh_calls(tier: ToolCallIdentityTier) {
    let world = World::new(&tier, "frames");
    let input = format!("tool-call identity law: root of {}", world.session_id);
    world.script(
        &input,
        vec![calls(&[(
            "call_0",
            PROBE,
            ProbeArgs {
                switch: true,
                ..ProbeArgs::label("frame-one")
            },
        )])],
    );
    world.script(
        &follow_on_task("frame-one"),
        vec![
            calls(&[("call_0", PROBE, ProbeArgs::label("frame-two"))]),
            text("answered in the follow-on frame"),
        ],
    );
    world
        .store()
        .await
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            world.session_id.clone(),
            crate::TurnInputIngress::NextTurn,
            crate::TurnInput::text(input),
        ))
        .await
        .expect("accept the root's input");
    let (report, mut reported) = tokio::sync::mpsc::unbounded_channel();
    world
        .runner()
        .run_crashed_then_redriven_turn(
            crate::admit(crate::ExecutionScope::queue_drain(
                &world.session_id,
                format!("{}-drive", world.session_id),
            )),
            root_attempt(&world, None),
            root_attempt(&world, Some(report)),
        )
        .await;
    let drive = reported
        .recv()
        .await
        .expect("the tier's runner ran the redriven root")
        .unwrap_or_else(|error| panic!("the redriven root runs: {error:?}"));
    let turn = drive.ran().expect("the redrive ran the root to its end");
    assert_finished("the redriven root", &turn);
    let before = only(&world, "frame-one");
    assert_one_identity("frame-two", &world.witness.of("frame-two"));
    let after = world.witness.of("frame-two").remove(0);
    assert_distinct_keys(
        "a first-frame call and a follow-on frame call whose provider emitted `call_0` each",
        &before.identity,
        &after.identity,
    );
}

/// A turn calls the probe as `call_0`; the session is compacted; a later turn
/// calls the probe as `call_0` again and dies after the call settled, and is
/// redriven. The later call is read back on the redrive and never runs
/// again, and its identity differs from the call before the compaction.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn compaction_keeps_identity_and_distinguishes_fresh_calls(tier: ToolCallIdentityTier) {
    let world = World::new(&tier, "compaction");
    let before = world.turn(
        "before",
        vec![
            calls(&[("call_0", PROBE, ProbeArgs::label("before"))]),
            text("before the compaction"),
        ],
    );
    assert_finished("the turn before the compaction", &world.run(&before).await);

    let (compacted, mut compaction) = tokio::sync::mpsc::unbounded_channel();
    let compact: crate::ConformanceTurnAttempt = {
        let world = world.clone();
        Arc::new(move |scope| {
            let world = world.clone();
            let compacted = compacted.clone();
            Box::pin(async move {
                let mut runtime = world.runtime(None).await;
                let outcome = Box::pin(runtime.compact_context(None, scope))
                    .await
                    .map_err(|error| error.to_string());
                let _ = compacted.send(outcome);
                crate::ConformanceTurnEnd::Settled
            })
        })
    };
    world
        .runner()
        .run_turn(
            crate::admit(crate::ExecutionScope::runtime_operation(format!(
                "{}-compaction",
                world.session_id
            ))),
            compact,
        )
        .await;
    assert_eq!(
        compaction
            .recv()
            .await
            .expect("the tier's runner ran the compaction"),
        Ok(true),
        "the session compacts"
    );

    let after = world.turn(
        "after",
        vec![
            calls(&[("call_0", PROBE, ProbeArgs::label("after"))]),
            text("after the compaction"),
        ],
    );
    let (report, mut reported) = tokio::sync::mpsc::unbounded_channel();
    let crashing: crate::ConformanceTurnAttempt = {
        let world = world.clone();
        let after = after.clone();
        Arc::new(move |scope| {
            let world = world.clone();
            let after = after.clone();
            Box::pin(async move {
                let ended = world
                    .drive(&after, scope, Some(Arc::new(PanicBeforeTurnCommit)))
                    .await;
                panic!("the crash probe did not fire before the turn commit: {ended:?}");
            })
        })
    };
    let calls_before = world.model_calls.load(Ordering::SeqCst);
    world
        .runner()
        .run_crashed_then_redriven_turn(
            world.admitted(&after),
            crashing,
            world.attempt(&after, report),
        )
        .await;
    let assembled = reported
        .recv()
        .await
        .expect("the redriven turn reports")
        .unwrap_or_else(|error| panic!("the redriven turn runs: {error}"));
    assert_finished("the redriven turn after the compaction", &assembled);
    assert_eq!(
        world.model_calls.load(Ordering::SeqCst) - calls_before,
        2,
        "the redrive reads the recorded model responses back instead of asking again"
    );
    let first = only(&world, "before");
    let second = only(&world, "after");
    assert_distinct_keys(
        "calls on either side of a compaction whose provider emitted `call_0` each",
        &first.identity,
        &second.identity,
    );
}

/// An RLM turn runs two code cells: the first calls the probe once, the
/// second calls it twice, the second of those holding after its effect, and
/// the turn dies there and is redriven. The replayed cells read their
/// recorded calls back and never run them again; the held call's every run
/// sees one call id; and the three calls are three identities, though the
/// same tool is called from the same kind of statement each time.
pub async fn code_cells_keep_identity_and_distinguish_fresh_calls(tier: ToolCallIdentityTier) {
    let world = World::code(&tier, "code-cells");
    let turn = world.turn(
        "turn",
        vec![
            cell(r#"await tools.identity_probe({ label: "cell-one" });"#),
            cell(
                r#"await tools.identity_probe({ label: "cell-two-a" });
finish(await tools.identity_probe({ label: "cell-two-b", hold: true }));"#,
            ),
        ],
    );
    let assembled = crash_while_held(&world, &turn, "cell-two-b").await;
    assert_finished("the redriven RLM turn", &assembled);
    let one = only(&world, "cell-one");
    let two_a = only(&world, "cell-two-a");
    assert_one_identity("cell-two-b", &world.witness.of("cell-two-b"));
    let two_b = world.witness.of("cell-two-b").remove(0);
    for (what, left, right) in [
        ("two cells' calls", &one, &two_a),
        ("two calls of one cell", &two_a, &two_b),
        ("the first cell's call and the held call", &one, &two_b),
    ] {
        assert_distinct_keys(what, &left.identity, &right.identity);
    }
}
