//! The fenced owner commit, including its page lease handoffs.
use super::*;
use lash_core_store::checkpoint_graph::GraphRefusal;
use lash_durable::domain::graph_commit::CheckedGraph;

/// Session plans are keyed by declared session/head and exact encoded content;
/// no list position supplies identity. Application keeps the original write
/// order and reaches every refusal under the original fences.
pub(super) type PreparedSessions = std::collections::BTreeMap<
    (lash_kernel_value::identity::SessionId, u64),
    std::collections::BTreeMap<
        String,
        Result<lash_core_store::store::RuntimeCommitPlanner, DurableError>,
    >,
>;

/// One immutable attempt, retained by the awaiting caller so its graph,
/// request bodies and plans are released off the database worker on success.
pub(super) struct PreparedOwner {
    pub(super) write: ActorTx,
    graph: Result<Option<PreparedGraph>, GraphRefusal>,
    sessions: PreparedSessions,
    fleet: lash_core_store::store::FleetFormat,
}

/// Pure envelope/leaf validation belongs to preparation. Its refusal is
/// consumed after the writer's fleet, actor and current-turn checks, exactly
/// where database-dependent environment authority is classified.
struct PreparedGraph {
    checked: CheckedGraph,
    environment: environment::SuppliedVersions,
}

pub(super) fn prepare(
    write: ActorTx,
    candidate: bool,
    fleet: lash_core_store::store::FleetFormat,
) -> Arc<PreparedOwner> {
    let graph = prepare_graph(&write, candidate);
    let mut sessions = PreparedSessions::new();
    for domain in write.domain() {
        if let DomainWrite::SessionCommit(write) = domain {
            sessions
                .entry((write.session.clone(), write.expected_head))
                .or_default()
                .entry(write.commit_json.clone())
                .or_insert_with(|| turns::prepare_session_commit(write, fleet));
        }
    }
    Arc::new(PreparedOwner {
        write,
        graph,
        sessions,
        fleet,
    })
}

/// Pure validation and sizing need no database lock. Keep their result
/// private to this attempted commit: the writer still applies refusals
/// after its fleet fence, actor fence and current-turn read, in that order.
fn prepare_graph(write: &ActorTx, candidate: bool) -> Result<Option<PreparedGraph>, GraphRefusal> {
    let publications: Vec<_> = write
        .domain()
        .iter()
        .flat_map(DomainWrite::publications)
        .collect();
    if !candidate
        && write.graph().is_empty()
        && write.reads().is_empty()
        && publications.is_empty()
        && !write
            .domain()
            .iter()
            .any(|w| matches!(w, DomainWrite::Environment(_)))
    {
        return Ok(None);
    }
    let checked = check_graph(write.graph(), write.reads(), &publications)?;
    // Candidate pages are bounded by their staging quota instead.
    if let Some(budget) = write.budget() {
        let measured = measure_owner_commit(write.graph(), write.domain())?;
        budget.admit(&measured)?;
    }
    let environment = lash_durable::environment::supplied_versions(&checked);
    Ok(Some(PreparedGraph {
        checked,
        environment,
    }))
}

pub(super) fn apply_owner(
    tx: &FencedTx<'_>,
    prepared: &PreparedOwner,
    now: DurableInstant,
    blob_profile: crate::BuiltinBlobProfile,
    candidate: Option<&mut CandidatePublication>,
) -> Flow<ActorCommit> {
    let write = &prepared.write;
    let fleet = tx.fleet();
    let actor = write.actor().as_str().to_owned();
    let fence = tx
        .prepare_cached(SQL.actor.fence.sql())?
        .query_row(
            rusqlite::params![actor, write.epoch().0, write.revision().0],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    let Some(revision) = fence else {
        return refuse(fenced(tx, write.actor(), write.epoch(), write.revision())?);
    };
    if write.domain().iter().any(|domain| {
        matches!(
            domain,
            DomainWrite::RegisterExecution(_)
                | DomainWrite::EndExecution(_)
                | DomainWrite::AdoptExecution(_)
                | DomainWrite::SeedResolution(_)
        )
    }) && revision - 1 != write.revision().0
    {
        return refuse(DurableError::ResolutionRevisionConflict {
            actor: write.actor().clone(),
            expected: write.revision(),
            current: StateRevision(revision - 1),
        });
    }
    // A session's unfinished turn's accepted cancel, read under the fence
    // before the commit writes: the receipt reports a cancel that arrived
    // after the owner's open.
    let (turn_cancel, open_roots) = match write.actor().kind() {
        lash_durable::ActorKind::Session => {
            let Ok(session) =
                lash_kernel_value::identity::SessionId::try_from(write.actor().id().to_owned())
            else {
                return refuse(corrupt("session id", write.actor().id()));
            };
            match turns::turn_frontier(tx, &session)? {
                Ok(Some((cancel, roots))) => (cancel, Some(roots)),
                Ok(None) => (None, None),
                Err(error) => return refuse(error),
            }
        }
        lash_durable::ActorKind::Process => (None, None),
    };
    let checked = match &prepared.graph {
        Ok(checked) => checked,
        Err(refusal) => return refuse(DurableError::from(refusal.clone())),
    };
    if let Some(prepared_graph) = checked {
        let checked = &prepared_graph.checked;
        let publishing = write
            .domain()
            .iter()
            .filter_map(|w| match w {
                DomainWrite::Environment(
                    lash_durable::environment::EnvironmentWrite::Publish { owner },
                ) => Some(owner.clone()),
                _ => None,
            })
            .collect();
        if let Err(error) =
            environment::validate_graph(tx, checked, &prepared_graph.environment, &publishing)?
        {
            return refuse(error);
        }
        if let Err(refusal) = graph::validate_exact_reads(tx, write.exact_reads())? {
            return refuse(DurableError::from(refusal));
        }
        let installed = match candidate {
            None => graph::install(tx, checked)?.map(drop),
            Some(candidate) => pages::install_candidate(tx, write.domain(), checked, candidate)?,
        };
        if let Err(refusal) = installed {
            return refuse(DurableError::from(refusal));
        }
    }
    let process_view = match super::process_view::refresh_preimage(tx, write)? {
        Ok(view) => view,
        Err(error) => return refuse(error),
    };
    let committing = Committing {
        process_views: lash_durable::domain::processes::ProcessPostimages::from_view(process_view),
        actor: write.actor(),
        epoch: write.epoch(),
        now,
        fleet,
        blob_profile,
        retired: Retired::default(),
        open_roots: turns::OpenRoots::read(open_roots),
    };
    let mut session_heads = Vec::new();
    for domain in write.domain() {
        let applied = match domain {
            DomainWrite::SessionCommit(head) => turns::apply_session_commit(
                tx,
                &committing,
                head,
                prepared
                    .sessions
                    .get(&(head.session.clone(), head.expected_head))
                    .and_then(|contents| contents.get(head.commit_json.as_str()))
                    .ok_or_else(|| {
                        crate::sqlite_conversion_error(crate::stored_data_corrupt(
                            "prepared owner commit",
                            "missing exact session/head/content",
                        ))
                    })?,
                prepared.fleet,
            )?
            .map(|receipt| {
                session_heads.push(lash_durable::CommittedSessionHead {
                    session: head.session.clone(),
                    receipt,
                });
            }),
            domain => apply_domain(tx, &committing, domain, &prepared.sessions, prepared.fleet)?,
        };
        if let Err(refusal) = applied {
            return refuse(refusal);
        }
        committing.process_views.applied(domain, write.epoch());
    }
    let process_views = committing.process_views.into_views();
    let mut root_leases = Vec::new();
    for request in write.root_leases() {
        match pages::acquire_in(tx, request.clone(), now)? {
            Ok(lease) => root_leases.push(lease),
            Err(error) => return refuse(error),
        }
    }
    if let Err(error) = environment::finish(tx, write.domain())? {
        return refuse(error);
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
    if write.requires().is_some()
        || write.domain().iter().any(|write| {
            matches!(
                write,
                DomainWrite::RegisterExecution(_)
                    | DomainWrite::EndExecution(_)
                    | DomainWrite::AdoptExecution(_)
                    | DomainWrite::SeedResolution(_)
                    | DomainWrite::Process(lash_durable::domain::ProcessWrite::Terminal { .. })
            )
        })
    {
        let requires = match packages::owner_requires(tx, write.actor(), write.requires())? {
            Ok(requires) => requires,
            Err(error) => return refuse(error),
        };
        processes::stamp_requires(tx, &actor, &requires)?;
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
    if write.actor().kind() == lash_durable::ActorKind::Session
        && !write.changes_follow_facts()
        && let Some(capture) = &tx.wake_capture
    {
        capture.unchanged_follow(write.actor());
    }
    commit(ActorCommit {
        process_views,
        session_heads,
        root_leases,
        revision: StateRevision(revision),
        state,
        turn_cancel,
    })
}
