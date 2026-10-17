//! A turn's model call identity (P1, FIG-5255; ADR 0133 §6).

use super::{LABEL, LawBroken, LawOutcome, create, ensure, node, session};
use crate::domain::{ModelCall, ModelPin, PhaseAdvance, RunSeq, TurnWrite, UnfinishedPhase};
use crate::{DomainWrite, DurableInstant, DurableStore};
use lash_core_store::store::{AdmittedTurnRows, RunAdmissionRecord};
use lash_protocol_types::{SessionId, TurnId};

/// A turn counts the model calls it admitted. Admission starts the count at
/// 0; each model phase stores its pin's call as the count, and the pin and
/// the row read back with it; a resend, the same call's next attempt, keeps
/// it; a tool round keeps it; and the next call, also within one protocol
/// iteration, reads back as the next ordinal. A restore therefore tells a
/// resend from a new call by the stored count alone.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_turn_counts_the_model_calls_it_admitted(store: &dyn DurableStore) -> LawOutcome {
    let id = SessionId::try_from("counted-session".to_owned())
        .map_err(|_| LawBroken("a constant session id".into()))?;
    let run = TurnId::try_from("counted-turn".to_owned())
        .map_err(|_| LawBroken("a constant turn id".into()))?;
    let actor = session(id.as_str())?;
    create(store, std::slice::from_ref(&actor)).await?;
    let owner = node(store, "count-owner").await?;
    let epoch = store.claim(&owner, 1).await?[0].epoch;
    let (actor, id) = (&actor, &id);
    let nodes = std::sync::Mutex::new(std::collections::BTreeMap::new());
    let nodes = &nodes;
    let commit = |write: TurnWrite| async move {
        let mut tx = store.begin(actor, epoch).await?;
        if let TurnWrite::Advance { phase, .. } = &write {
            let node = nodes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&phase.checkpoint())
                .cloned();
            tx.stage(node);
        }
        tx.write(DomainWrite::Turn(write));
        store.commit(tx, LABEL).await?;
        Ok::<_, LawBroken>(())
    };
    let checkpoint = |tag: &str| {
        let (root, node) = super::tagged_node(tag);
        nodes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root, node);
        root
    };
    let model = |call: u32, attempt: u32| PhaseAdvance::Model {
        pin: ModelPin {
            call,
            attempt,
            request_ref: format!("request-{call}"),
            deadline: DurableInstant(1_000),
            stream_from: format!("stream-{call}"),
        },
        checkpoint: checkpoint(&format!("checkpoint-{call}-{attempt}")),
    };
    let advance = |phase: PhaseAdvance| TurnWrite::Advance {
        session: id.clone(),
        run: run.clone(),
        phase,
        iteration: 1,
    };
    let read = || async move {
        let row = store
            .turn(id)
            .await?
            .ok_or_else(|| LawBroken("the admitted turn vanished".into()))?;
        Ok::<_, LawBroken>((row.model_calls, row.phase.model().map(|pin| pin.call)))
    };

    commit(TurnWrite::Admit {
        session: id.clone(),
        run: run.clone(),
        admission: RunAdmissionRecord::Turn {
            took: AdmittedTurnRows::Inputs {
                ids: lash_core_store::store::AdmittedInputIds::one(
                    lash_protocol_types::InputId::from("counted-batch"),
                ),
            },
            trace: None,
        },
        turn_deadline: None,
    })
    .await?;
    let read_back = read().await?;
    ensure!(read_back == (0, None), "admission read back {read_back:?}");

    let steps = [
        (model(1, 1), (1, Some(1)), "the first call"),
        (model(1, 2), (1, Some(1)), "the first call's resend"),
        (
            PhaseAdvance::Tools {
                run: RunSeq(1),
                checkpoint: checkpoint("round"),
            },
            (1, None),
            "a tool round",
        ),
        (model(2, 1), (2, Some(2)), "the second call"),
        (model(3, 1), (3, Some(3)), "a third call in one iteration"),
    ];
    for (phase, expected, name) in steps {
        commit(advance(phase.clone())).await?;
        let read_back = read().await?;
        ensure!(
            read_back == expected,
            "{name} read back {read_back:?}, not {expected:?}"
        );
        let stored = store.turn(id).await?.map(|row| row.phase);
        let phase = UnfinishedPhase::from(phase);
        ensure!(
            stored.as_ref() == Some(&phase),
            "{name} stored {stored:?}, not {phase:?}"
        );
    }
    Ok(())
}

/// A model call's receipt stands beside its model phase (FIG-6253): the
/// unfinished turn records it only in its model phase, once, leaving the
/// phase and its root as they were; the receipt is the root its row names
/// until the turn's next phase consumes it, and the turn's end takes it
/// with the phase row.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_receipt_stands_beside_its_model_phase_until_the_next_phase(
    store: &dyn DurableStore,
) -> LawOutcome {
    let id = SessionId::try_from("receipt-session".to_owned())
        .map_err(|_| LawBroken("a constant session id".into()))?;
    let run = TurnId::try_from("receipt-turn".to_owned())
        .map_err(|_| LawBroken("a constant turn id".into()))?;
    let actor = session(id.as_str())?;
    create(store, std::slice::from_ref(&actor)).await?;
    let owner = node(store, "receipt-owner").await?;
    let epoch = store.claim(&owner, 1).await?[0].epoch;
    let (actor, id, run) = (&actor, &id, &run);
    let commit = |write: TurnWrite,
                  node: Option<lash_core_store::checkpoint_graph::EncodedNode>| async move {
        let mut tx = store.begin(actor, epoch).await?;
        tx.stage(node);
        tx.write(DomainWrite::Turn(write));
        store.commit(tx, LABEL).await.map(|_| ())
    };
    let answer = |root| TurnWrite::Answer {
        session: id.clone(),
        run: run.clone(),
        answer: root,
    };
    let refused = |result: Result<(), crate::DurableError>| {
        matches!(
            result,
            Err(crate::DurableError::Domain(
                crate::domain::DomainRefusal::TurnNotOpen { .. }
            ))
        )
    };
    let row = || async move {
        store
            .turn(id)
            .await?
            .ok_or_else(|| LawBroken("the admitted turn vanished".into()))
    };
    let receipt = || async move {
        store
            .hydrate(
                crate::domain::RootOwner::ModelAnswer {
                    session: id.clone(),
                    run: run.clone(),
                },
                None,
                lash_core_store::checkpoint_graph::DecodeLimits::within_total(1 << 20),
            )
            .await
            .map(|graph| graph.root)
    };
    let retired = |found: Result<_, crate::DurableError>| {
        matches!(
            found,
            Err(crate::DurableError::Graph(
                lash_core_store::checkpoint_graph::GraphRefusal::RootRetired
            ))
        )
    };

    commit(
        TurnWrite::Admit {
            session: id.clone(),
            run: run.clone(),
            admission: RunAdmissionRecord::Turn {
                took: AdmittedTurnRows::Inputs {
                    ids: lash_core_store::store::AdmittedInputIds::one(
                        lash_protocol_types::InputId::from("receipt-batch"),
                    ),
                },
                trace: None,
            },
            turn_deadline: None,
        },
        None,
    )
    .await?;
    let (first, first_node) = super::tagged_node("receipt-1");
    ensure!(
        refused(commit(answer(first), Some(first_node.clone())).await),
        "an admitted turn took a receipt"
    );

    let (checkpoint, checkpoint_node) = super::tagged_node("receipt-phase");
    let model = PhaseAdvance::Model {
        pin: ModelPin {
            call: 1,
            attempt: 1,
            request_ref: "request-1".to_owned(),
            deadline: DurableInstant(1_000),
            stream_from: "stream-1".to_owned(),
        },
        checkpoint,
    };
    let advance = |phase: PhaseAdvance| TurnWrite::Advance {
        session: id.clone(),
        run: run.clone(),
        phase,
        iteration: 1,
    };
    commit(advance(model.clone()), Some(checkpoint_node)).await?;
    ensure!(
        row().await?.phase == UnfinishedPhase::from(model.clone()) && retired(receipt().await),
        "a model phase held a receipt before one was recorded"
    );
    commit(answer(first), Some(first_node)).await?;
    let answered = row().await?;
    let PhaseAdvance::Model { pin, checkpoint } = model.clone() else {
        unreachable!("the phase is a model phase");
    };
    ensure!(
        answered.phase
            == UnfinishedPhase::Model {
                pin,
                checkpoint,
                call: ModelCall::Answered { receipt: first },
            },
        "the receipt read back as {:?}",
        answered.phase
    );
    ensure!(
        receipt().await? == first,
        "the receipt's root is not the receipt"
    );
    let (second, second_node) = super::tagged_node("receipt-2");
    ensure!(
        refused(commit(answer(second), Some(second_node)).await),
        "a receipt was written twice"
    );
    ensure!(
        row().await?.phase.receipt() == Some(first),
        "a refused receipt replaced the first"
    );

    let (round, round_node) = super::tagged_node("receipt-round");
    commit(
        advance(PhaseAdvance::Tools {
            run: RunSeq(1),
            checkpoint: round,
        }),
        Some(round_node),
    )
    .await?;
    ensure!(
        row().await?.phase.receipt().is_none() && retired(receipt().await),
        "the next phase did not consume the receipt"
    );
    let (third, third_node) = super::tagged_node("receipt-3");
    ensure!(
        refused(commit(answer(third), Some(third_node.clone())).await),
        "a tool round took a receipt"
    );

    let (next, next_node) = super::tagged_node("receipt-next-phase");
    commit(
        advance(PhaseAdvance::Model {
            pin: ModelPin {
                call: 2,
                attempt: 1,
                request_ref: "request-2".to_owned(),
                deadline: DurableInstant(1_000),
                stream_from: "stream-2".to_owned(),
            },
            checkpoint: next,
        }),
        Some(next_node),
    )
    .await?;
    // FIG-6617: one fenced transaction must observe its own root writes.
    // Answer invalidates the cached roots; each Advance consumes its cache.
    let (transient, transient_node) = super::tagged_node("receipt-transient");
    let (final_root, final_node) = super::tagged_node("receipt-final");
    let mut tx = store.begin(actor, epoch).await?;
    tx.stage([third_node.clone(), transient_node]);
    tx.write(DomainWrite::Turn(answer(third)));
    tx.write(DomainWrite::Turn(TurnWrite::AddToolCalls {
        session: id.clone(),
        run: run.clone(),
        calls: 3,
    }));
    tx.write(DomainWrite::Turn(advance(PhaseAdvance::Tools {
        run: RunSeq(2),
        checkpoint: transient,
    })));
    store.commit(tx, LABEL).await?;
    // A new transaction begins with populated fenced roots. The first
    // Advance must consume those roots before the second Advance runs.
    let (middle, middle_node) = super::tagged_node("receipt-middle");
    let mut tx = store.begin(actor, epoch).await?;
    tx.stage([middle_node, final_node]);
    tx.write(DomainWrite::Turn(advance(PhaseAdvance::Tools {
        run: RunSeq(3),
        checkpoint: middle,
    })));
    tx.write(DomainWrite::Turn(TurnWrite::AddToolCalls {
        session: id.clone(),
        run: run.clone(),
        calls: 2,
    }));
    tx.write(DomainWrite::Turn(advance(PhaseAdvance::Tools {
        run: RunSeq(4),
        checkpoint: final_root,
    })));
    store.commit(tx, LABEL).await?;
    let final_row = row().await?;
    ensure!(
        final_row.phase.checkpoint() == Some(final_root) && final_row.tool_calls == 5,
        "the ordered batch lost its final phase or tool admissions: {final_row:?}"
    );
    ensure!(
        retired(receipt().await),
        "the batched advance left its receipt readable"
    );
    let pages = store
        .pages()
        .ok_or_else(|| LawBroken("the store has no page retirement".into()))?;
    while pages
        .retire_step(crate::domain::pages::RetirementBudget { nodes: 128 })
        .await?
        != crate::domain::pages::RetirementProgress::default()
    {}
    // No orphan audit: retirement itself must release every superseded root.
    // Republish without supplying a body: a freed root must be refused.
    for freed in [first, checkpoint, round, next, third, transient, middle] {
        ensure!(
            matches!(commit(advance(PhaseAdvance::Tools { run: RunSeq(4), checkpoint: freed }), None).await,
            Err(crate::DurableError::Graph(lash_core_store::checkpoint_graph::GraphRefusal::MissingNode(found))) if found == freed),
            "a batched write leaked the retired root {freed:?}"
        );
    }
    // A refusal after an increment and an advance rolls back both the phase
    // and the retirements; the final closure must remain readable.
    let (rolled_back, rolled_back_node) = super::tagged_node("receipt-rolled-back");
    let mut tx = store.begin(actor, epoch).await?;
    tx.stage([rolled_back_node, third_node]);
    tx.write(DomainWrite::Turn(TurnWrite::AddToolCalls {
        session: id.clone(),
        run: run.clone(),
        calls: 9,
    }));
    tx.write(DomainWrite::Turn(advance(PhaseAdvance::Tools {
        run: RunSeq(5),
        checkpoint: rolled_back,
    })));
    tx.write(DomainWrite::Turn(answer(third)));
    ensure!(
        refused(store.commit(tx, LABEL).await.map(|_| ())),
        "a refused batch committed"
    );
    ensure!(
        row().await? == final_row,
        "a refusal changed the committed phase or count"
    );
    let live = store
        .hydrate(
            crate::domain::RootOwner::TurnPhase {
                session: id.clone(),
                run: run.clone(),
            },
            None,
            lash_core_store::checkpoint_graph::DecodeLimits::within_total(1 << 20),
        )
        .await?;
    ensure!(
        live.root == final_root,
        "a refusal retired the final closure"
    );
    // Keep the original terminal case too: end an answered model phase,
    // so the terminal itself must take an unconsumed receipt with it.
    let (terminal_phase, terminal_phase_node) = super::tagged_node("receipt-terminal-phase");
    commit(
        advance(PhaseAdvance::Model {
            pin: ModelPin {
                call: 3,
                attempt: 1,
                request_ref: "request-3".to_owned(),
                deadline: DurableInstant(1_000),
                stream_from: "stream-3".to_owned(),
            },
            checkpoint: terminal_phase,
        }),
        Some(terminal_phase_node),
    )
    .await?;
    let (terminal_answer, terminal_answer_node) = super::tagged_node("receipt-terminal-answer");
    commit(answer(terminal_answer), Some(terminal_answer_node)).await?;
    ensure!(
        receipt().await? == terminal_answer,
        "the terminal control has no live receipt"
    );
    commit(
        TurnWrite::Terminal {
            session: id.clone(),
            run: run.clone(),
            cause: Box::new(
                lash_core_store::store::RunTerminalCause::OperatorCancelled {
                    intent: lash_core_store::store::ControlIntentId::from_sequence(1),
                },
            ),
            head_revision: None,
            transcript: None,
        },
        None,
    )
    .await?;
    ensure!(
        store.turn(id).await?.is_none() && retired(receipt().await),
        "the turn's end left its receipt"
    );
    Ok(())
}

/// The tool count is part of the owner commit: an advance preserves it,
/// and a refused commit rolls its increment back with every other write.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_turn_counts_its_tool_admissions_atomically(store: &dyn DurableStore) -> LawOutcome {
    let id = SessionId::try_from("tool-count-session".to_owned())
        .map_err(|_| LawBroken("session id".into()))?;
    let run =
        TurnId::try_from("tool-count-turn".to_owned()).map_err(|_| LawBroken("turn id".into()))?;
    let actor = session(id.as_str())?;
    create(store, std::slice::from_ref(&actor)).await?;
    let owner = node(store, "tool-count-owner").await?;
    let epoch = store.claim(&owner, 1).await?[0].epoch;
    let mut tx = store.begin(&actor, epoch).await?;
    tx.write(DomainWrite::Turn(TurnWrite::Admit {
        session: id.clone(),
        run: run.clone(),
        admission: RunAdmissionRecord::Turn {
            took: AdmittedTurnRows::Inputs {
                ids: lash_core_store::store::AdmittedInputIds::one("tool-count-input".into()),
            },
            trace: None,
        },
        turn_deadline: None,
    }));
    store.commit(tx, LABEL).await?;
    let count = || async {
        store
            .turn(&id)
            .await?
            .map(|row| row.tool_calls)
            .ok_or_else(|| LawBroken("the turn vanished".into()))
    };
    ensure!(count().await? == 0, "new turn has no tool admissions");
    let increment = |calls| {
        DomainWrite::Turn(TurnWrite::AddToolCalls {
            session: id.clone(),
            run: run.clone(),
            calls,
        })
    };
    let mut tx = store.begin(&actor, epoch).await?;
    tx.write(increment(2));
    tx.write(increment(3));
    store.commit(tx, LABEL).await?;
    ensure!(count().await? == 5, "the commit lost an increment");
    let mut tx = store.begin(&actor, epoch).await?;
    tx.write(increment(7));
    // A receipt is refused outside a model phase, after the increment ran.
    let (root, body) = super::tagged_node("refused-tool-count");
    tx.stage(Some(body));
    tx.write(DomainWrite::Turn(TurnWrite::Answer {
        session: id.clone(),
        run: run.clone(),
        answer: root,
    }));
    ensure!(
        matches!(
            store.commit(tx, LABEL).await,
            Err(crate::DurableError::Domain(_))
        ),
        "the invalid receipt committed"
    );
    ensure!(count().await? == 5, "a refused commit counted calls");
    Ok(())
}
