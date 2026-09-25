//! FIG-3672 P9: a turn learns its cancellation only from recorded peeks, so a
//! cancel that arrives after a peek recorded "no stop" never reaches a replay
//! of that peek. Proven with the determinism harness over the real
//! [`ActiveTurnControl`] peeks, against an engine double whose gate is live on
//! a fresh run and served from the journal on every replay.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use lash_sansio::sync::MutexExt;

use super::*;
use crate::engine::testing::{
    DeterminismCheck, FailureCause, LocalEngine, LocalTestCx, ReplayMode, RunMode,
};
use crate::{
    AdmittedScope, EffectGroupHandle, GroupSettlement, LoserPolicy, RecordedJournal,
    RecordedKeyRange, RecordedKeys, RuntimeEffectController, RuntimeEffectGroup,
};

/// The gate pair's live state, shared by every run of one check: what an
/// external requester has resolved so far, by promise key.
#[derive(Clone, Default)]
struct LiveGate(Arc<Mutex<BTreeMap<String, Resolution>>>);

impl LiveGate {
    fn resolve(&self, key: &AwaitEventKey, terminal: impl Serialize) {
        self.0.lock_recover().insert(
            key.key_id.clone(),
            gate_resolution(terminal).expect("encode the gate terminal"),
        );
    }

    fn get(&self, key: &AwaitEventKey) -> Option<Resolution> {
        self.0.lock_recover().get(&key.key_id).cloned()
    }
}

/// An engine double over the harness: every effect is one recorded operation,
/// and a gate peek's body reads the live gate.
struct GateEngine<'c> {
    cx: &'c LocalTestCx,
    gate: LiveGate,
}

#[async_trait::async_trait]
impl AwaitEventResolver for GateEngine<'_> {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        Some("gate-engine".to_string())
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        Ok(AwaitEventKey {
            key_id: format!("{}:{wait:?}", scope.id()),
            scope: scope.clone(),
            wait,
            signature: "gate-engine".to_string(),
        })
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for GateEngine<'_> {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        _local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let canonical = envelope.canonical_form()?;
        let key = envelope.invocation.replay_key().to_string();
        let kind = envelope.command.kind().as_str().to_string();
        let RuntimeEffectCommand::PeekAwaitEvent { key: peeked } = envelope.command else {
            panic!("the gate engine serves gate peeks only");
        };
        let gate = self.gate.clone();
        self.cx
            .op_with_command_bytes(key, kind, canonical.json().to_string(), async move {
                Ok(RuntimeEffectOutcome::PeekAwaitEvent {
                    resolution: gate.get(&peeked),
                })
            })
            .await
    }

    async fn open_effect_group(
        &self,
        _group: RuntimeEffectGroup,
    ) -> Result<EffectGroupHandle, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("the gate engine"))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut EffectGroupHandle,
        _cancel: crate::runtime::TurnCancelWait,
    ) -> Result<GroupSettlement, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("the gate engine"))
    }

    async fn close_effect_group(
        &self,
        _handle: EffectGroupHandle,
        _disposition: LoserPolicy,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("the gate engine"))
    }

    async fn read_recorded_journal(
        &self,
        _range: &RecordedKeyRange,
    ) -> Result<RecordedJournal, crate::RuntimeEffectControllerError> {
        Ok(RecordedJournal::Keys(RecordedKeys::default()))
    }
}

fn address() -> TurnAddress {
    TurnAddress::new("gate-session", "gate-turn")
}

fn evidence(request_id: &str, mode: TurnCancelMode) -> TurnCancellationEvidence {
    TurnCancellationEvidence {
        request_id: request_id.to_string(),
        origin: None,
        reason: None,
        undelivered: TurnCancelDisposition::Defer,
        mode,
        honoured_after_step: None,
    }
}

/// What a requester does while the turn runs, after the turn's `n`-th peek.
type Arrival = fn(&GateEngine<'_>, &ActiveTurnControl, usize);

/// A code cell's checkpoints, then the after-cell peek: the turn records the
/// peek at which it first honours a stop, and the evidence it honours.
fn cell_drive(
    gate: LiveGate,
    arrival: Arrival,
) -> impl for<'c> Fn(&'c (), &'c LocalTestCx) -> Pin<Box<dyn Future<Output = ()> + 'c>>
+ Send
+ Sync
+ 'static {
    move |_, cx| {
        let gate = gate.clone();
        Box::pin(async move {
            let engine = GateEngine { cx, gate };
            let control = ActiveTurnControl::new(&engine, address())
                .await
                .expect("key the gate pair");
            let scoped = ScopedEffectController::borrowed(
                &engine,
                AdmittedScope::turn("gate-session", "gate-turn"),
            )
            .expect("admit the turn");
            let mut honoured = None;
            for checkpoint in 1..=4_u64 {
                let observed = control
                    .observe_pending_cancel(
                        &scoped,
                        TurnCancelPeekIdentity::CellCheckpoint {
                            cell: "turn:1:0:exec_code:1".to_string(),
                            checkpoint,
                        },
                    )
                    .await
                    .expect("peek the gate at a checkpoint");
                arrival(&engine, &control, checkpoint as usize);
                if let Some(evidence) = observed {
                    honoured = Some((checkpoint, evidence));
                    break;
                }
            }
            cx.record_commit(&honoured);
        })
    }
}

fn check(gate: LiveGate, arrival: Arrival) -> crate::engine::testing::DeterminismReport {
    let engine = LocalEngine::new(|| (), cell_drive(gate, arrival));
    DeterminismCheck::new(0x3672_0009)
        .perturbed_replays(6)
        .run(&engine)
        .unwrap_or_else(|failure| panic!("{failure}"))
}

/// A stop that lands after the second checkpoint recorded "no stop" is
/// honoured at the third on the fresh run; every replay honours it at the
/// third too, although the live gate already holds it before the first.
#[test]
fn a_cancel_arriving_during_a_replay_window_takes_the_recorded_path() {
    fn arrival(engine: &GateEngine<'_>, control: &ActiveTurnControl, after: usize) {
        if after == 2 && !engine.cx.is_replaying() {
            engine.gate.resolve(
                &control.cancel_key,
                TurnGateTerminal::CancelRequested(evidence("late", TurnCancelMode::Immediate)),
            );
        }
    }
    let report = check(LiveGate::default(), arrival);
    assert_eq!(
        report.transcript.commits().collect::<Vec<_>>(),
        vec![r#"[3,{"request_id":"late"}]"#],
    );
}

/// The escalation: an `AfterStep` request never stops a cell mid-run, and the
/// escalation that follows it stops the cell at the next checkpoint, with the
/// escalating request's identity and the accepted policy.
#[test]
fn an_escalated_after_step_request_stops_the_cell_at_its_next_checkpoint() {
    fn arrival(engine: &GateEngine<'_>, control: &ActiveTurnControl, after: usize) {
        if engine.cx.is_replaying() {
            return;
        }
        match after {
            1 => engine.gate.resolve(
                &control.cancel_key,
                TurnGateTerminal::CancelRequested(TurnCancellationEvidence {
                    undelivered: TurnCancelDisposition::Drop,
                    ..evidence("stop", TurnCancelMode::AfterStep)
                }),
            ),
            2 => engine.gate.resolve(
                &control.escalation_key,
                TurnEscalationTerminal::Escalated(TurnEscalationEvidence::from(&evidence(
                    "abort",
                    TurnCancelMode::Immediate,
                ))),
            ),
            _ => {}
        }
    }
    let report = check(LiveGate::default(), arrival);
    assert_eq!(
        report.transcript.commits().collect::<Vec<_>>(),
        vec![r#"[3,{"request_id":"abort","undelivered":"drop"}]"#],
    );
}

/// The shape this replaced: a turn that reads the live gate, rather than its
/// recorded peek, decides differently on a replay once a cancel has arrived.
#[test]
fn a_turn_reading_the_live_gate_diverges_on_replay() {
    let gate = LiveGate::default();
    let engine = LocalEngine::new(|| (), {
        let gate = gate.clone();
        move |_: &(), cx: &LocalTestCx| {
            let gate = gate.clone();
            Box::pin(async move {
                let engine = GateEngine { cx, gate };
                let control = ActiveTurnControl::new(&engine, address())
                    .await
                    .expect("key the gate pair");
                let live = engine.gate.get(&control.cancel_key).is_some();
                if !cx.is_replaying() {
                    engine.gate.resolve(
                        &control.cancel_key,
                        TurnGateTerminal::CancelRequested(evidence(
                            "late",
                            TurnCancelMode::Immediate,
                        )),
                    );
                }
                cx.record_commit(&live);
            }) as Pin<Box<dyn Future<Output = ()> + '_>>
        }
    });
    let failure = DeterminismCheck::new(0x3672_0009)
        .run(&engine)
        .expect_err("a live read is not recorded");
    assert_eq!(failure.mode, RunMode::Replay(ReplayMode::Cold));
    assert!(
        matches!(failure.cause, FailureCause::Diverged(_)),
        "{failure}"
    );
}
