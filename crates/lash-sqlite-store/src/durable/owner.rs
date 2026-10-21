//! The fenced owner commit, including its page lease handoffs.
use super::*;

pub(super) fn apply_owner(
    tx: &FencedTx<'_>,
    write: ActorTx,
    now: DurableInstant,
    blob_profile: crate::BuiltinBlobProfile,
    candidate: Option<&mut CandidatePublication>,
) -> Flow<ActorCommit> {
    let fleet = tx.fleet();
    let actor = write.actor().as_str().to_owned();
    let fence = tx
        .prepare_cached(SQL.actor.fence.sql())?
        .query_row(rusqlite::params![actor, write.epoch().0], |row| {
            row.get::<_, i64>(0)
        })
        .optional()?;
    let Some(revision) = fence else {
        return refuse(fenced(tx, write.actor(), write.epoch())?);
    };
    // A session's unfinished turn's accepted cancel, read under the fence
    // before the commit writes: the receipt reports a cancel that arrived
    // after the owner's open.
    let (turn_cancel, open_roots) = match write.actor().kind() {
        lash_durable::ActorKind::Session => {
            let Ok(session) =
                lash_protocol_types::SessionId::try_from(write.actor().id().to_owned())
            else {
                return refuse(corrupt("session id", write.actor().id()));
            };
            match turns::turn_with_roots(tx, &session)? {
                Ok(Some((row, roots))) => (row.cancel, Some(roots)),
                Ok(None) => (None, None),
                Err(error) => return refuse(error),
            }
        }
        lash_durable::ActorKind::Process => (None, None),
    };
    let publications: Vec<_> = write
        .domain()
        .iter()
        .flat_map(DomainWrite::publications)
        .collect();
    if candidate.is_some()
        || !write.graph().is_empty()
        || !write.reads().is_empty()
        || !publications.is_empty()
    {
        let checked = match check_graph(write.graph(), write.reads(), &publications) {
            Ok(checked) => checked,
            Err(refusal) => return refuse(DurableError::from(refusal)),
        };
        // The budget admits the commit's own graph and writes; a
        // candidate's pages are bounded by its staging quota instead.
        if let Some(budget) = write.budget() {
            let admitted = measure_owner_commit(write.graph(), write.domain())
                .and_then(|measured| budget.admit(&measured));
            if let Err(refusal) = admitted {
                return refuse(DurableError::from(refusal));
            }
        }
        let installed = match candidate {
            None => graph::install(tx, &checked)?.map(drop),
            Some(candidate) => pages::install_candidate(tx, write.domain(), &checked, candidate)?,
        };
        if let Err(refusal) = installed {
            return refuse(DurableError::from(refusal));
        }
    }
    let committing = Committing {
        actor: write.actor(),
        epoch: write.epoch(),
        now,
        fleet,
        blob_profile,
        retired: Retired::default(),
        open_roots: turns::OpenRoots::read(open_roots),
    };
    for domain in write.domain() {
        if let Err(refusal) = apply_domain(tx, &committing, domain)? {
            return refuse(refusal);
        }
    }
    let mut root_leases = Vec::new();
    for request in write.root_leases() {
        match pages::acquire_in(tx, request.clone(), now)? {
            Ok(lease) => root_leases.push(lease),
            Err(error) => return refuse(error),
        }
    }
    for lease in write.released_root_leases() {
        if let Some(root) = pages::release_in(tx, *lease)? {
            committing.retired.stored(root);
        }
    }
    graph::hold_retired(tx, committing.retired, now)?;
    if let Some(formats) = write.formats() {
        cached_execute(
            tx,
            SQL.actor.stamp_formats.sql(),
            rusqlite::params![actor, formats.as_str()],
        )?;
    }
    if let Some(requires) = write.requires() {
        processes::stamp_requires(tx, &actor, requires)?;
    }
    if let Some(through) = write.ack() {
        cached_execute(tx, SQL.actor.ack.sql(), rusqlite::params![actor, through.0])?;
        cached_execute(
            tx,
            SQL.mail.delete_through.sql(),
            rusqlite::params![actor, through.0],
        )?;
    }
    let state = match write.release() {
        None => ActorState::Owned,
        Some(Release::Parked) => {
            // A stop that reached the actor since the owner's read is not
            // lost to the park: the actor goes ready instead, its park kept,
            // and its claimer runs the stop (FIG-6099).
            let stopped = write.immediate_cancel_since_read(turn_cancel.as_ref())
                || tx
                    .prepare_cached(SQL.park.stop_mail_after.sql())?
                    .query_row(
                        rusqlite::params![
                            actor,
                            write.seen().0,
                            lash_durable::domain::CANCEL_MAIL,
                            lash_durable::domain::SESSION_CLOSE_MAIL
                        ],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
            let stored: String = if stopped {
                tx.prepare_cached(SQL.sqlite.release_without_due.sql())?
                    .query_row(
                        rusqlite::params![actor, "ready", Option::<i64>::None, now.0],
                        |row| row.get(0),
                    )?
            } else {
                tx.prepare_cached(SQL.park.park.sql())?
                    .query_row([&actor], |row| row.get(0))?
            };
            match actor_state(&stored) {
                Ok(state) => state,
                Err(error) => return refuse(error),
            }
        }
        Some(Release::Terminal) => {
            cached_execute(tx, SQL.mail.delete_all.sql(), [&actor])?;
            let stored: String = tx
                .prepare_cached(SQL.actor.end.sql())?
                .query_row([&actor], |row| row.get(0))?;
            match actor_state(&stored) {
                Ok(state) => state,
                Err(error) => return refuse(error),
            }
        }
        Some(rest) => {
            let (state, due) = match rest {
                Release::Waiting { next_due } => ("waiting", next_due.map(|due| due.0)),
                Release::Ready => ("ready", None),
                _ => ("idle", None),
            };
            let stored: String = tx
                .prepare_cached(if due.is_some() {
                    SQL.actor.release.sql()
                } else {
                    SQL.sqlite.release_without_due.sql()
                })?
                .query_row(rusqlite::params![actor, state, due, now.0], |row| {
                    row.get(0)
                })?;
            if rest == Release::Ready && !write.advances() {
                cached_execute(tx, SQL.actor.unadvanced.sql(), [&actor])?;
            }
            match actor_state(&stored) {
                Ok(state) => state,
                Err(error) => return refuse(error),
            }
        }
    };
    commit(ActorCommit {
        root_leases,
        revision: StateRevision(revision),
        state,
        turn_cancel,
    })
}
