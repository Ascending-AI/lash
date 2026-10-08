//! A turn's model call identity (P1, FIG-5255; ADR 0133 §6).

use super::{LABEL, LawBroken, LawResult, create, ensure, node, session};
use crate::domain::{ModelPin, RunSeq, TurnWrite, UnfinishedPhase};
use crate::{DomainWrite, DurableInstant, DurableStore};
use lash_core_store::store::{AdmittedTurnRows, RunAdmissionRecord};
use lash_sansio::{SessionId, TurnId};

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
pub async fn a_turn_counts_the_model_calls_it_admitted(store: &dyn DurableStore) -> LawResult {
    let id = SessionId::try_from("counted-session".to_owned())
        .map_err(|_| LawBroken("a constant session id".into()))?;
    let run = TurnId::try_from("counted-turn".to_owned())
        .map_err(|_| LawBroken("a constant turn id".into()))?;
    let actor = session(id.as_str())?;
    create(store, std::slice::from_ref(&actor)).await?;
    let owner = node(store, "count-owner").await?;
    let epoch = store.claim(&owner, 1).await?[0].epoch;
    let (actor, id) = (&actor, &id);
    let commit = |write: TurnWrite| async move {
        let mut tx = store.begin(actor, epoch).await?;
        tx.write(DomainWrite::Turn(write));
        store.commit(tx, LABEL).await?;
        Ok::<_, LawBroken>(())
    };
    let model = |call: u32, attempt: u32| UnfinishedPhase::Model {
        pin: ModelPin {
            call,
            attempt,
            request_ref: format!("request-{call}"),
            deadline: DurableInstant(1_000),
            stream_from: format!("stream-{call}"),
        },
        checkpoint: format!("checkpoint-{call}-{attempt}"),
    };
    let advance = |phase: UnfinishedPhase| TurnWrite::Advance {
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
                ids: lash_core_store::store::AdmittedInputIds::one(lash_sansio::InputId::from(
                    "counted-batch",
                )),
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
            UnfinishedPhase::Tools {
                run: RunSeq(1),
                checkpoint: "round".to_owned(),
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
        ensure!(
            stored.as_ref() == Some(&phase),
            "{name} stored {stored:?}, not {phase:?}"
        );
    }
    Ok(())
}
