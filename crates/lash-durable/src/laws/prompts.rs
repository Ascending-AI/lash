//! Prompt snapshot roots and their shared text (P2, FIG-5256; ADR 0133 §5).

use super::{LABEL, LawBroken, LawOutcome, create, ensure, node, session};
use crate::domain::{
    DomainRefusal, ModelCallId, PromptCallKey, PromptSnapshotRow, PromptText, PromptWrite,
    TurnWrite,
};
use crate::{DomainWrite, DurableError, DurableStore, Epoch};
use lash_core_store::store::{
    AdmittedTurnRows, ControlIntentId, RunAdmissionRecord, RunTerminalCause, SessionCatalogStore,
};
use lash_vocabulary::identity::SessionId;
use lash_vocabulary::identity::TurnId;

fn text(name: &str) -> PromptText {
    PromptText {
        hash: lash_core_store::store::BlobRef::for_content(format!("text {name}").as_bytes()).0,
        text: format!("text {name}").into(),
        children: Vec::new(),
    }
}

fn sorted_hashes(names: &[&str]) -> Vec<String> {
    let mut hashes = hashes(names);
    hashes.sort();
    hashes
}

fn hashes(names: &[&str]) -> Vec<String> {
    names
        .iter()
        .map(|name| {
            lash_core_store::store::BlobRef::for_content(format!("text {name}").as_bytes()).0
        })
        .collect()
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
) -> LawOutcome {
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
        call: ModelCallId::Turn {
            run: run.clone(),
            ordinal: 1,
        },
    };
    let record = |run: &TurnId, names: &[&str]| {
        DomainWrite::Prompt(PromptWrite::Record {
            call: call(run),
            snapshot: Some(format!("snapshot of {run}")),
            request: format!("request of {run}"),
            snapshot_texts: names.iter().map(|name| text(name)).collect(),
            request_texts: Default::default(),
            prefix: None,
        })
    };
    let admit = |run: &TurnId| {
        DomainWrite::Turn(TurnWrite::Admit {
            session: id.clone(),
            run: run.clone(),
            admission: RunAdmissionRecord::Turn {
                took: AdmittedTurnRows::Inputs {
                    ids: lash_core_store::store::AdmittedInputIds::one(
                        lash_vocabulary::identity::InputId::from(if *run == first {
                            "first-batch"
                        } else {
                            "second-batch"
                        }),
                    ),
                },
                trace: None,
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
            transcript: None,
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
        root.snapshot == Some(format!("snapshot of {first}"))
            && root.snapshot_texts == sorted_hashes(&["a", "b"]),
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

/// Deleting a session is the explicit retention its prompt snapshots wait
/// for (FIG-5272). The delete releases every root of the session, in each of
/// its turns, and reclaims every text only those roots referenced; a text
/// another live session still roots survives with that session's root.
///
/// # Errors
///
/// The first rule broken.
pub async fn deleting_a_session_releases_its_prompt_roots_and_keeps_shared_text(
    store: &dyn DurableStore,
    catalog: &dyn SessionCatalogStore,
) -> LawOutcome {
    let id = |name: &str| {
        SessionId::try_from(name.to_owned()).map_err(|_| LawBroken(format!("session id {name}")))
    };
    let turn = |name: &str| {
        TurnId::try_from(name.to_owned()).map_err(|_| LawBroken(format!("turn id {name}")))
    };
    let (deleted, live) = (id("deleted-session")?, id("live-session")?);
    let actors = [session(deleted.as_str())?, session(live.as_str())?];
    create(store, &actors).await?;
    let owner = node(store, "prompt-owner").await?;
    let claimed = store.claim(&owner, actors.len()).await?;
    ensure!(claimed.len() == 2, "the sessions were not both claimed");
    let call = |session: &SessionId, run: &str, call: u32| {
        Ok::<_, LawBroken>(PromptCallKey {
            session: session.clone(),
            call: ModelCallId::Turn {
                run: turn(run)?,
                ordinal: call,
            },
        })
    };
    // A compaction's call, owned by its execution with no turn, goes with
    // its session too.
    let owned = PromptCallKey {
        session: deleted.clone(),
        call: ModelCallId::Owned {
            owner: "session-operation:compaction".to_owned(),
            key: "summary".to_owned(),
        },
    };
    let calls = [
        (call(&deleted, "first-turn", 1)?, &["own", "shared"][..]),
        (call(&deleted, "first-turn", 2)?, &["own"][..]),
        (call(&deleted, "second-turn", 1)?, &["later"][..]),
        (owned, &["own", "summary"][..]),
        (call(&live, "first-turn", 1)?, &["shared", "kept"][..]),
    ];
    for (call, names) in &calls {
        let actor = session(call.session.as_str())?;
        let Some(owned) = claimed.iter().find(|claimed| claimed.actor == actor) else {
            return Err(LawBroken(format!("{} was not claimed", call.session)));
        };
        let mut tx = store.begin(&owned.actor, owned.epoch).await?;
        let texts = names.iter().map(|name| text(name)).collect::<Vec<_>>();
        // The owned call is open: its root holds its request alone.
        let (snapshot, snapshot_texts, request_texts) = match call.call {
            ModelCallId::Owned { .. } => (None, Vec::new(), texts),
            ModelCallId::Turn { .. } => (
                Some(format!("snapshot of {}", call.call)),
                texts,
                Vec::new(),
            ),
        };
        tx.write(DomainWrite::Prompt(PromptWrite::Record {
            call: call.clone(),
            snapshot,
            request: format!("request of {}", call.call),
            snapshot_texts,
            request_texts: request_texts.into(),
            prefix: None,
        }));
        store.commit(tx, LABEL).await?;
    }

    catalog
        .delete_session(&deleted)
        .await
        .map_err(|failure| LawBroken(format!("the delete failed: {failure:?}")))?;
    for (call, _) in &calls[..4] {
        ensure!(
            store.prompt_snapshot(call).await?.is_none(),
            "the deleted session kept the root of {call:?}"
        );
    }
    let root = store
        .prompt_snapshot(&calls[4].0)
        .await?
        .ok_or_else(|| LawBroken("deleting one session released another's root".into()))?;
    ensure!(
        root.snapshot_texts == sorted_hashes(&["kept", "shared"]),
        "the live root reads back as {root:?}"
    );
    let stored = store
        .prompt_texts(&hashes(&["own", "shared", "later", "summary", "kept"]))
        .await?;
    ensure!(
        stored == [text("shared"), text("kept")],
        "after the delete the stored texts are {stored:?}"
    );
    Ok(())
}

/// Settling a turn's calls releases each one's request and keeps its
/// snapshot (FIG-5946). The commit that admits the next call settles the
/// open one but keeps the new call's request, so a text both requests hold
/// survives; the commit that leaves the model phase settles them all. A
/// settled request's edges go, and with them every text no other edge
/// names: a text the snapshot or another turn's call also references stays.
/// A root that held only its request goes with it, and a later record whose
/// text was reclaimed stores that text again.
///
/// # Errors
///
/// The first rule broken.
pub async fn settling_a_turns_calls_releases_their_requests_and_keeps_their_snapshots(
    store: &dyn DurableStore,
) -> LawOutcome {
    let id = SessionId::try_from("settled-session".to_owned())
        .map_err(|_| LawBroken("a constant session id".into()))?;
    let turn = |name: &str| {
        TurnId::try_from(name.to_owned()).map_err(|_| LawBroken(format!("turn id {name}")))
    };
    let (run, other) = (turn("settled-turn")?, turn("other-turn")?);
    let actor = session(id.as_str())?;
    create(store, std::slice::from_ref(&actor)).await?;
    let actor = &actor;
    let owner = node(store, "prompt-owner").await?;
    let claimed = store.claim(&owner, 1).await?;
    ensure!(claimed.len() == 1, "the session was not claimed");
    let epoch = claimed[0].epoch;
    let call = |run: &TurnId, ordinal: u32| PromptCallKey {
        session: id.clone(),
        call: ModelCallId::Turn {
            run: run.clone(),
            ordinal,
        },
    };
    let names = |names: &[&str]| names.iter().map(|name| text(name)).collect::<Vec<_>>();
    let record = |key: PromptCallKey, snapshot: &[&str], request: &[&str]| {
        DomainWrite::Prompt(PromptWrite::Record {
            snapshot: (!snapshot.is_empty()).then(|| format!("snapshot of {}", key.call)),
            request: format!("request of {}", key.call),
            snapshot_texts: names(snapshot),
            request_texts: names(request).into(),
            prefix: None,
            call: key,
        })
    };
    let settle = |open: Option<u32>| {
        DomainWrite::Prompt(PromptWrite::Settle {
            session: id.clone(),
            run: run.clone(),
            open,
        })
    };
    let commit = |writes: Vec<DomainWrite>| async move {
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
    let root = |key: PromptCallKey| async move {
        store
            .prompt_snapshot(&key)
            .await?
            .ok_or_else(|| LawBroken(format!("{} lost its root", key.call)))
    };
    let all = ["s", "dual", "body1", "shared", "elsewhere", "body2"];

    // `model.start` of call 1, and another turn's open call.
    commit(vec![
        record(
            call(&run, 1),
            &["s", "dual"],
            &["body1", "shared", "dual", "elsewhere"],
        ),
        record(call(&other, 1), &[], &["elsewhere"]),
    ])
    .await?;
    // `model.start` of call 2 settles call 1, the one still open.
    commit(vec![
        record(call(&run, 2), &["s"], &["shared", "body2"]),
        settle(Some(2)),
    ])
    .await?;
    let first = root(call(&run, 1)).await?;
    ensure!(
        first
            == PromptSnapshotRow {
                call: call(&run, 1),
                snapshot: Some(format!("snapshot of {}", call(&run, 1).call)),
                request: None,
                snapshot_texts: sorted_hashes(&["dual", "s"]),
                request_texts: Vec::new(),
                written_epoch: first.written_epoch,
            },
        "the settled call's root reads back as {first:?}"
    );
    let second = root(call(&run, 2)).await?;
    ensure!(
        second.request.is_some() && second.request_texts == sorted_hashes(&["body2", "shared"]),
        "settling call 1 released the request of call 2, the call its commit admits: {second:?}"
    );
    ensure!(
        stored(&all).await? == hashes(&["s", "dual", "shared", "elsewhere", "body2"]),
        "after call 1 settled the store holds {:?}",
        stored(&all).await?
    );

    // `model.done` settles every call of the turn.
    commit(vec![settle(None)]).await?;
    let second = root(call(&run, 2)).await?;
    ensure!(
        second.request.is_none()
            && second.request_texts.is_empty()
            && second.snapshot_texts == sorted_hashes(&["s"]),
        "the settled call 2 reads back as {second:?}"
    );
    ensure!(
        stored(&all).await? == hashes(&["s", "dual", "elsewhere"]),
        "after the turn settled the store holds {:?}",
        stored(&all).await?
    );
    ensure!(
        root(call(&other, 1)).await?.request.is_some(),
        "settling one turn released another turn's request"
    );

    // A call with no sections, over a text the turn's settlement reclaimed.
    commit(vec![record(call(&run, 3), &[], &["body1"])]).await?;
    let texts = store.prompt_texts(&hashes(&["body1"])).await?;
    ensure!(
        texts == [text("body1")],
        "a reclaimed text recorded again reads back as {texts:?}"
    );
    commit(vec![settle(None)]).await?;
    ensure!(
        store.prompt_snapshot(&call(&run, 3)).await?.is_none(),
        "a root that held only its request survived its settlement"
    );
    ensure!(
        stored(&["body1"]).await?.is_empty(),
        "the settled bare root's text survived"
    );
    Ok(())
}
