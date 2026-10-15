//! A boot's redrive of the session parks another build may clear
//! (FIG-6310).

use super::{LABEL, LawBroken, LawOutcome, create, ensure, node, session};
use crate::domain::{DomainWrite, SessionParksRedrive};
use crate::{
    ActorKey, ActorState, CommitLabel, DurableStore, MailAnswer, MailDomainWrite, MailTx, Release,
};

/// Park `actor`, claimed at once by `owner`, with the encoded `park`.
async fn park(
    store: &dyn DurableStore,
    owner: &crate::NodeLease,
    actor: &ActorKey,
    park: &str,
) -> LawOutcome {
    let claimed = store.claim(owner, 1).await?;
    ensure!(
        claimed.len() == 1 && &claimed[0].actor == actor,
        "{actor} was not the one claim: {claimed:?}"
    );
    let mut tx = store.begin(actor, claimed[0].epoch).await?;
    tx.write(DomainWrite::ActorPark {
        reason_json: Some(park.to_owned()),
    });
    tx.give_up(Release::Parked);
    store.commit(tx, LABEL).await?;
    Ok(())
}

/// A boot's redrive readies exactly the session actors parked with a reason
/// it names, each as an operator's redrive would: its park and failed
/// activations cleared and its mailbox position taken, so its next claim
/// runs it. A session parked for another reason, a process parked with a
/// named reason's tag, and an actor that is not parked are left as they
/// were.
pub async fn a_boot_redrives_only_the_session_parks_it_names(
    store: &dyn DurableStore,
) -> LawOutcome {
    let named = session("park-named")?;
    let other = session("park-other")?;
    let process = ActorKey::process("park-process").map_err(|e| LawBroken(e.to_string()))?;
    let ready = session("park-ready")?;
    let owner = node(store, "parker").await?;
    let parks = [
        (&named, r#"{"reason":"pass_loop","failed_passes":8}"#),
        (
            &other,
            r#"{"reason":"activation_loop","failed_activations":8}"#,
        ),
        (&process, r#"{"reason":"pass_loop","failed_passes":8}"#),
    ];
    for (actor, encoded) in parks {
        create(store, std::slice::from_ref(actor)).await?;
        park(store, &owner, actor, encoded).await?;
    }
    create(store, std::slice::from_ref(&ready)).await?;
    let before = store
        .actor(&named)
        .await?
        .ok_or_else(|| LawBroken("the named session vanished".into()))?;

    let mut tx = MailTx::new();
    tx.write(MailDomainWrite::RedriveSessionParks(SessionParksRedrive {
        reasons: vec!["pass_loop".to_owned(), "undecodable_state".to_owned()],
    }));
    let commit = store
        .commit_mail(tx, CommitLabel::new("law.redrive"))
        .await?;
    ensure!(
        commit.answers == [MailAnswer::RedriveSessionParks(vec![named.clone()])],
        "the redrive answered {:?}, not the named session alone",
        commit.answers
    );
    ensure!(
        commit.woken.len() == 1
            && commit.woken[0].actor == named
            && commit.woken[0].state == ActorState::Ready,
        "the redrive woke {:?}",
        commit.woken
    );

    let after = store
        .actor(&named)
        .await?
        .ok_or_else(|| LawBroken("the named session vanished".into()))?;
    ensure!(
        after.state == ActorState::Ready
            && after.park.is_none()
            && after.failed_activations == 0
            && after.has_mail
            && after.epoch == before.epoch,
        "the named session was not redriven as an operator's redrive leaves it: {after:?}"
    );
    for (actor, encoded) in [&parks[1], &parks[2]] {
        let row = store
            .actor(actor)
            .await?
            .ok_or_else(|| LawBroken(format!("{actor} vanished")))?;
        ensure!(
            row.state == ActorState::Parked && row.park.as_deref() == Some(*encoded),
            "{actor} was redriven: {row:?}"
        );
    }
    let claimed = store.claim(&owner, 4).await?;
    let mut claimed: Vec<_> = claimed.into_iter().map(|claim| claim.actor).collect();
    claimed.sort();
    let mut expected = vec![named, ready];
    expected.sort();
    ensure!(
        claimed == expected,
        "the next claim took {claimed:?}, not the redriven session and the ready one"
    );
    Ok(())
}
