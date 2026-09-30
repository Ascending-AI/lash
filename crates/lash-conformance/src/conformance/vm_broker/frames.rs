//! The frame-open law's session: one owner's frames on the tier.

use std::sync::Arc;
use std::time::Duration;

use lash_vm_broker::testing::{FakeWorkerPool, MemoryCheckpoints, ScriptedProgram, Step};
use lash_vm_broker::{
    Broker, BrokerFailure, BrokeredEnd, Checkpoint, CheckpointStore as _, RunStart, VmSession,
};
use lash_vm_protocol::{FrameEpoch, OwnerEpoch};
use tokio_util::sync::CancellationToken;

use super::{CHECKOUT_WAIT, Phase, Scenario, TierEffects, bounds, codec, contract, echo, limits};
use crate::ScopedEffectController;

/// What the frame-open law observed.
pub(super) struct FrameOutcome {
    /// The first frame's committed completion.
    pub(super) first_frame: Checkpoint,
    /// How the first frame's run still on a worker when the frame opened
    /// ended.
    pub(super) straggler: Result<BrokeredEnd, BrokerFailure>,
    /// Whether its worker went back to the pool discarded.
    pub(super) straggler_discarded: bool,
    /// Every checkpoint committed before the new frame's run.
    pub(super) commits: Vec<Checkpoint>,
    /// What the store held right after the frame opened.
    pub(super) persisted_after_open: Option<Checkpoint>,
    /// The new frame's run's results.
    pub(super) read: Vec<serde_json::Value>,
}

fn start(program: Vec<Step>, from: Option<Checkpoint>) -> RunStart {
    RunStart {
        program: ScriptedProgram::new(program).source(),
        contexts: Vec::new(),
        limits: limits(),
        from,
    }
}

/// Frame 0 sets a global and completes; a second run of frame 0 is left
/// computing on its worker; frame 1 opens; a run of frame 1 reads the
/// global.
pub(super) async fn open_frame_under(
    scenario: &Scenario,
    scoped: ScopedEffectController<'_>,
) -> FrameOutcome {
    let session = VmSession::new(FrameEpoch(0));
    let checkpoints = MemoryCheckpoints::default();
    let pool = Arc::new(FakeWorkerPool::new(codec(), 2, CHECKOUT_WAIT));
    let context = scenario.context(&scoped);
    let effects = TierEffects {
        scoped,
        probe: Arc::clone(&scenario.probe),
        phase: Phase::Healthy,
        pool: Arc::clone(&pool),
        stop_after_checkpoint: None,
        stop: CancellationToken::new(),
    };
    let broker = Broker {
        context: &context,
        effects: &effects,
        checkpoints: &checkpoints,
        slots: pool.as_ref(),
        codec: codec(),
        contract: contract(),
        bounds: bounds(),
        frames: session.frames().clone(),
    };
    let stop = CancellationToken::new();
    let first = broker
        .run(
            start(
                vec![
                    Step::SetGlobal {
                        name: "planted".into(),
                        value: serde_json::json!("frame-0 secret"),
                    },
                    Step::Invoke(echo(1)),
                ],
                None,
            ),
            &stop,
        )
        .await;
    let Ok(BrokeredEnd::Complete {
        checkpoint: first_frame,
        ..
    }) = first
    else {
        panic!("the first frame's run completes: {first:?}");
    };
    let discards_before = pool.stats().discards;
    let straggler = broker.run(
        start(
            vec![Step::Invoke(echo(2)), Step::Hang],
            Some(first_frame.clone()),
        ),
        &stop,
    );
    let open = async {
        // The straggler's call was answered, and its worker computes on.
        while pool.stats().hung == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        session
            .open_frame(FrameEpoch(1), &checkpoints, Duration::from_secs(30))
            .await
            .unwrap_or_else(|failure| panic!("the frame opens: {failure}"));
        checkpoints
            .latest()
            .await
            .unwrap_or_else(|refusal| panic!("read the store: {refusal}"))
    };
    let (straggler, persisted_after_open) = tokio::join!(straggler, open);
    let straggler_discarded = pool.stats().discards > discards_before;
    let commits = checkpoints.commits();
    pool.set_epochs(OwnerEpoch(0), FrameEpoch(1));
    let from = checkpoints
        .latest()
        .await
        .unwrap_or_else(|refusal| panic!("read the store: {refusal}"));
    let read = match broker
        .run(
            start(
                vec![Step::ReadGlobal {
                    name: "planted".into(),
                }],
                from,
            ),
            &stop,
        )
        .await
    {
        Ok(BrokeredEnd::Complete { value, .. }) => lash_vm_broker::authority::decode_value(&value)
            .ok()
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default(),
        other => panic!("the new frame's run completes: {other:?}"),
    };
    FrameOutcome {
        first_frame,
        straggler,
        straggler_discarded,
        commits,
        persisted_after_open,
        read,
    }
}
