//! Prompt snapshot roots and their shared text (P2, FIG-5256; ADR 0133 §5).

use super::{LABEL, LawBroken, LawResult, create, ensure, node, session};
use crate::domain::{DomainRefusal, PromptCallKey, PromptText, PromptWrite, TurnWrite};
use crate::{DomainWrite, DurableError, DurableStore, Epoch};
use lash_core_store::store::{
    AdmittedTurnRows, ControlIntentId, RunAdmissionRecord, RunTerminalCause,
};
use lash_sansio::{SessionId, TurnId};

fn text(name: &str) -> PromptText {
    PromptText {
        hash: format!("hash-{name}"),
        text: format!("text {name}"),
    }
}

fn hashes(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| format!("hash-{name}")).collect()
}

/// A call's prompt snapshot is an audit root. Recording it stores each text
/// once, shared with every other root naming it; recording the same call
/// again is refused and writes nothing. Ending the turn, which prunes its
/// phase rows, keeps the root and its texts. Only the explicit release
/// removes roots: releasing one turn keeps every text another root still
/// shares, and releasing the session leaves nothing.
///
/// # Errors
///
/// The first rule broken.
pub async fn prompt_snapshot_roots_survive_phase_pruning_until_released(
    store: &dyn DurableStore,
) -> LawResult {
    let id = SessionId::try_from("prompted-session".to_owned())
        .map_err(|_| LawBroken("a constant session id".into()))?;
    let turn = |name: &str| {
        TurnId::try_from(name.to_owned()).map_err(|_| LawBroken(format!("turn id {name}")))
    };
    let (first, second) = (turn("first-turn")?, turn("second-turn")?);
    let actor = session(id.as_str())?;
    create(store, std::slice::from_ref(&actor)).await?;
    let actor = &actor;
    let owner = node(store, "prompt-owner").await?;
    let claimed = store.claim(&owner, 1).await?;
    ensure!(claimed.len() == 1, "the session was not claimed");
    let epoch = claimed[0].epoch;
    let call = |run: &TurnId| PromptCallKey {
        session: id.clone(),
        run: run.clone(),
        call: 1,
    };
    let record = |run: &TurnId, names: &[&str]| {
        DomainWrite::Prompt(PromptWrite::Record {
            call: call(run),
            snapshot: format!("snapshot of {run}"),
            texts: names.iter().map(|name| text(name)).collect(),
        })
    };
    let admit = |run: &TurnId| {
        DomainWrite::Turn(TurnWrite::Admit {
            session: id.clone(),
            run: run.clone(),
            admission: RunAdmissionRecord::Turn {
                took: AdmittedTurnRows::Batch {
                    id: lash_sansio::BatchId::from(if *run == first {
                        "first-batch"
                    } else {
                        "second-batch"
                    }),
                },
            },
            turn_deadline: None,
        })
    };
    let end = |run: &TurnId| {
        DomainWrite::Turn(TurnWrite::Terminal {
            session: id.clone(),
            run: run.clone(),
            cause: Box::new(RunTerminalCause::OperatorCancelled {
                intent: ControlIntentId::from_sequence(1),
            }),
            head_revision: None,
        })
    };
    let commit = |writes: Vec<DomainWrite>, epoch: Epoch| async move {
        let mut tx = store.begin(actor, epoch).await?;
        for write in writes {
            tx.write(write);
        }
        store.commit(tx, LABEL).await
    };
    let stored = |names: &[&str]| {
        let wanted = hashes(names);
        async move {
            let found = store.prompt_texts(&wanted).await?;
            Ok::<_, DurableError>(found.into_iter().map(|text| text.hash).collect::<Vec<_>>())
        }
    };

    commit(vec![admit(&first), record(&first, &["a", "b"])], epoch).await?;
    match commit(vec![record(&first, &["c"])], epoch).await {
        Err(DurableError::Domain(DomainRefusal::PromptCallRecorded { call: refused }))
            if refused == call(&first) => {}
        other => {
            return Err(LawBroken(format!(
                "a second record of one call answered {other:?}"
            )));
        }
    }
    ensure!(
        stored(&["c"]).await?.is_empty(),
        "a refused record stored its text"
    );

    commit(vec![end(&first)], epoch).await?;
    ensure!(
        store.turn(&id).await?.is_none(),
        "the ended turn kept its phase row"
    );
    let root = store
        .prompt_snapshot(&call(&first))
        .await?
        .ok_or_else(|| LawBroken("ending the turn pruned its prompt root".into()))?;
    ensure!(
        root.snapshot == format!("snapshot of {first}") && root.texts == hashes(&["a", "b"]),
        "the root reads back as {root:?}"
    );
    ensure!(
        stored(&["a", "b"]).await? == hashes(&["a", "b"]),
        "ending the turn pruned its prompt text"
    );

    commit(
        vec![admit(&second), record(&second, &["b", "c"]), end(&second)],
        epoch,
    )
    .await?;
    let texts = store.prompt_texts(&hashes(&["b"])).await?;
    ensure!(
        texts == [text("b")],
        "the shared text reads back as {texts:?}"
    );

    commit(
        vec![DomainWrite::Prompt(PromptWrite::Release {
            session: id.clone(),
            run: Some(first.clone()),
        })],
        epoch,
    )
    .await?;
    ensure!(
        store.prompt_snapshot(&call(&first)).await?.is_none(),
        "the released root survived"
    );
    ensure!(
        stored(&["a", "b", "c"]).await? == hashes(&["b", "c"]),
        "releasing one turn left texts {:?}",
        stored(&["a", "b", "c"]).await?
    );
    ensure!(
        store.prompt_snapshot(&call(&second)).await?.is_some(),
        "releasing one turn released another"
    );

    commit(
        vec![DomainWrite::Prompt(PromptWrite::Release {
            session: id.clone(),
            run: None,
        })],
        epoch,
    )
    .await?;
    ensure!(
        store.prompt_snapshot(&call(&second)).await?.is_none(),
        "releasing the session left a root"
    );
    ensure!(
        stored(&["a", "b", "c"]).await?.is_empty(),
        "releasing the session left texts"
    );
    Ok(())
}
