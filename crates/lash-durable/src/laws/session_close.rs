//! The session closing state's rows (L6b, FIG-5176; ADR 0132 §12).

use super::{LABEL, LawBroken, LawOutcome, create, node, session};
use crate::domain::{DomainRefusal, SessionCloseStep, SessionCloseWrite};
use crate::{DomainWrite, DurableError, DurableStore, Release};
use lash_sansio::SessionId;

fn session_id(id: &str) -> Result<SessionId, LawBroken> {
    SessionId::parse(id).map_err(|error| LawBroken(error.to_string()))
}

/// A session close is recorded one step at a time, in order: each step is
/// the one after the stored step, a step out of order is refused and writes
/// nothing, a second begin keeps the close where it was, and after the last
/// step the row stays as the session's tombstone. A crash between steps
/// therefore resumes at exactly the step after the stored one.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_session_close_moves_one_step_at_a_time(store: &dyn DurableStore) -> LawOutcome {
    let actor = session("closing")?;
    let id = session_id("closing")?;
    create(store, std::slice::from_ref(&actor)).await?;
    let owner = node(store, "close-owner").await?;
    let epoch = store.claim(&owner, 1).await?[0].epoch;
    let write = |write: SessionCloseWrite| DomainWrite::SessionClose(write);
    let step = |step: SessionCloseStep| {
        write(SessionCloseWrite::Step {
            session: id.clone(),
            step,
        })
    };

    let mut tx = store.begin(&actor, epoch).await?;
    tx.write(step(SessionCloseStep::Cancel));
    match store.commit(tx, LABEL).await {
        Err(DurableError::Domain(DomainRefusal::SessionNotClosing { session })) => {
            super::ensure!(session == id, "the refusal named {session}, not {id}");
        }
        other => {
            return Err(LawBroken(format!(
                "a step before the close began answered {other:?}"
            )));
        }
    }
    super::ensure!(
        store.session_close(&id).await?.is_none(),
        "a refused step wrote a close row"
    );

    let mut tx = store.begin(&actor, epoch).await?;
    tx.write(write(SessionCloseWrite::Begin {
        session: id.clone(),
    }));
    store.commit(tx, LABEL).await?;
    let begun = store
        .session_close(&id)
        .await?
        .ok_or_else(|| LawBroken("the begun close has no row".into()))?;
    super::ensure!(
        begun.done.is_none() && begun.next() == Some(SessionCloseStep::Cancel),
        "a begun close resumes at {:?}, not the cancel",
        begun.next()
    );

    let revision = store.actor(&actor).await?.map(|snapshot| snapshot.revision);
    let mut tx = store.begin(&actor, epoch).await?;
    tx.write(step(SessionCloseStep::Revoke));
    match store.commit(tx, LABEL).await {
        Err(DurableError::Domain(DomainRefusal::SessionCloseOutOfOrder {
            step: SessionCloseStep::Revoke,
            done: None,
            ..
        })) => {}
        other => {
            return Err(LawBroken(format!(
                "a step skipping the cancel answered {other:?}"
            )));
        }
    }
    super::ensure!(
        store.actor(&actor).await?.map(|snapshot| snapshot.revision) == revision
            && store.session_close(&id).await? == Some(begun.clone()),
        "a refused step wrote something"
    );

    for done in SessionCloseStep::ALL {
        let mut tx = store.begin(&actor, epoch).await?;
        tx.write(step(done));
        if done == SessionCloseStep::Cancel {
            // A second close request drained while closing is the same close.
            tx.write(write(SessionCloseWrite::Begin {
                session: id.clone(),
            }));
        }
        if done == SessionCloseStep::Tombstone {
            tx.give_up(Release::Terminal);
        }
        store.commit(tx, done.label()).await?;
        let row = store
            .session_close(&id)
            .await?
            .ok_or_else(|| LawBroken(format!("the close row vanished at {done:?}")))?;
        super::ensure!(
            row.done == Some(done) && row.begun_at == begun.begun_at,
            "after {done:?} the row is {row:?}"
        );
        super::ensure!(
            row.next() == done.next(),
            "after {done:?} the close resumes at {:?}",
            row.next()
        );
    }
    let tombstone = store
        .session_close(&id)
        .await?
        .ok_or_else(|| LawBroken("the tombstone vanished".into()))?;
    super::ensure!(
        tombstone.is_tombstone(),
        "the finished close is not a tombstone"
    );
    Ok(())
}
