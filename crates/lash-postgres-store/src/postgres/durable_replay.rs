//! A durable commit's one retry owner (FIG-5242):
//! [`DurableStore::commit`](lash_durable::DurableStore::commit) and
//! [`DurableStore::commit_mail`](lash_durable::DurableStore::commit_mail) run their immutable write under the
//! label's [`RetryPolicy`](crate::host::RetryPolicy), each attempt from a
//! fresh `BEGIN` through the writer fence and, for an owner, its epoch
//! fence, within the role's one operation deadline. A `COMMIT` whose answer
//! was lost is reconciled from the transaction's recorded outcome
//! ([`crate::replayable`]), never run again blind.

use lash_durable::domain::DomainWrite;
use lash_durable::{
    ActorCommit, ActorState, ActorTx, CommitCapacity, CommitLabel, DurableError, DurableInstant,
    MailCommit, MailTx, Release, StoreFailure, StoreFailureKind,
};

use lash_durable::domain::pages::CandidatePublication;

use super::{
    Committing, PostgresDurableStore, Retired, SQL, Tx, actor_state, apply_domain, apply_mail,
    executor, fenced, get, graph, group_members, packages, pages, sqlx_failure, store_failure,
};
use crate::guarded_tx::begin_durable;
use crate::replayable::{Attempt, Settle, Uncommitted, XactId};

/// A refusal as a replayable commit attempt's failure: contention rolled
/// the attempt back, and anything else (an ownership fence included) is the
/// commit's answer.
pub(super) fn attempt(error: DurableError) -> Attempt<DurableError> {
    if matches!(
        &error,
        DurableError::Store(StoreFailure {
            kind: StoreFailureKind::Contended,
            ..
        })
    ) {
        Attempt::Aborted(error)
    } else {
        Attempt::Failed(error)
    }
}

impl PostgresDurableStore {
    /// [`DurableStore::commit`](lash_durable::DurableStore::commit): the owner's write, applied after its
    /// ownership fence, run again while the database rolls it back for
    /// contention.
    pub(super) async fn commit_owner(
        &self,
        tx: ActorTx,
        label: CommitLabel,
    ) -> Result<ActorCommit, DurableError> {
        self.commit_owner_with(tx, label, None).await
    }

    /// [`commit_owner`](Self::commit_owner) publishing `candidate`
    /// (FIG-6191, FIG-7576): the candidate's pages stream in bounded batches.
    /// A candidate of one batch rides in the commit beside the owner's own
    /// graph ([`pages::riding_candidate`]); a larger one installs ahead, each
    /// attempt first, and the owner's write then names only its root.
    /// Pages are immutable and content-addressed, so installing them ahead
    /// of the commit is idempotent: an attempt that never commits leaves
    /// pages no root or edge reaches, which the orphan audit collects.
    pub(super) async fn commit_owner_with(
        &self,
        tx: ActorTx,
        label: CommitLabel,
        candidate: Option<&tokio::sync::Mutex<CandidatePublication>>,
    ) -> Result<ActorCommit, DurableError> {
        let deadline = self.deadline(label.capacity());
        let root = match candidate {
            Some(candidate) => Some(candidate.lock().await.manifest.summary().root),
            None => None,
        };
        // A candidate of one batch rides in the commit; a larger one
        // installs ahead of it, every attempt.
        let riding = match candidate {
            Some(candidate) => pages::riding_candidate(&tx, &mut *candidate.lock().await)
                .map_err(DurableError::Graph)?,
            None => None,
        };
        let ahead = candidate.filter(|_| riding.is_none());
        let riding = riding.as_ref();
        self.within(
            label.capacity(),
            lash_core_ids::operational_metrics::sql::with_owner(
                tx.actor().as_str().to_owned(),
                tx.epoch().0,
                tx.revision().0,
                async {
                    Box::pin(crate::observed_sql::measure(
                        &self.observer,
                        label,
                        group_members(&tx),
                        async {
                            if tx.ack().is_some_and(|through| through > tx.seen()) {
                                return Err(DurableError::AckBeyondRead {
                                    actor: tx.actor().clone(),
                                });
                            }
                            let tx = &tx;
                            let call = executor::Call::of(
                                tx,
                                riding,
                                &self.pools.held_texts,
                                &self.pools.session_hints,
                                self.fence.fleet(),
                            );
                            let call = &call;
                            self.replay(label, deadline, || async move {
                                if let Some(candidate) = ahead {
                                    self.install_pages(label, tx, &mut *candidate.lock().await)
                                        .await?;
                                }
                                if let Some(call) = call
                                    && let Some(commit) =
                                        self.call_owner(label, tx, call, deadline).await?
                                {
                                    return Ok(commit);
                                }
                                let (mut guarded, now, fence) =
                                    self.open_replayable(label, Some(tx)).await?;
                                let outcome = Box::pin(apply_owner(
                                    &mut guarded,
                                    tx,
                                    riding,
                                    fence,
                                    now,
                                    self.fence.fleet(),
                                    &self.pools,
                                ))
                                .await;
                                // The orphan audit took the root the
                                // attempt installed: install it again.
                                let outcome = match outcome {
                                    Err(DurableError::Graph(
                                        lash_core_store::checkpoint_graph::GraphRefusal::MissingNode(
                                            missing,
                                        ),
                                    )) if Some(missing) == root => {
                                        Err(DurableError::Store(StoreFailure {
                                            code: None,
                                            kind: StoreFailureKind::Contended,
                                            message: format!(
                                                "the candidate root {missing} was collected \
                                                 before its commit"
                                            ),
                                        }))
                                    }
                                    outcome => outcome,
                                };
                                self.settle(label, guarded, outcome, deadline).await
                            })
                            .await
                            .inspect(|_| self.pools.held_texts.note(tx))
                        },
                    ))
                    .await
                },
            ),
        )
        .await
    }

    /// Install `candidate`'s pages for owner commit `tx`, in one
    /// transaction of its own under the owner's fence: its manifest streamed
    /// in bounded batches of nodes, then of edges, every node the stream
    /// names checked stored ([`pages::install_owned_candidate`]).
    async fn install_pages(
        &self,
        label: CommitLabel,
        tx: &ActorTx,
        candidate: &mut CandidatePublication,
    ) -> Result<(), Attempt<DurableError>> {
        let publications: Vec<_> = tx
            .domain()
            .iter()
            .flat_map(DomainWrite::publications)
            .collect();
        let checked =
            lash_durable::domain::graph_commit::check_graph(tx.graph(), tx.reads(), &publications)
                .map_err(|refusal| Attempt::Failed(DurableError::Graph(refusal)))?;
        let (mut pages_tx, _) = self.open(label).await.map_err(attempt)?;
        let outcome = pages::install_owned_candidate(&mut pages_tx, tx, &checked, candidate).await;
        super::finish(pages_tx, outcome).await.map_err(attempt)
    }

    /// [`DurableStore::commit_mail`](lash_durable::DurableStore::commit_mail): the mailbox write, run again while the
    /// database rolls it back for contention.
    pub(super) async fn commit_mailbox(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        let deadline = self.deadline(label.capacity());
        self.within(label.capacity(), async {
            crate::observed_sql::measure(&self.observer, label, 0, async {
                let tx = &tx;
                self.replay(label, deadline, || async move {
                    let (mut guarded, now, _) = self.open_replayable(label, None).await?;
                    let outcome =
                        Box::pin(apply_mail(&mut guarded, tx, now, self.fence.fleet())).await;
                    self.settle(label, guarded, outcome, deadline).await
                })
                .await
            })
            .await
        })
        .await
    }

    /// When an operation on `capacity` starting now must have finished: its
    /// role's whole-operation deadline, which its retries share.
    fn deadline(&self, capacity: CommitCapacity) -> Option<tokio::time::Instant> {
        self.route(capacity)
            .prelude
            .deadline()
            .map(|deadline| tokio::time::Instant::now() + deadline)
    }

    /// Run one replayable commit `attempt` under `label`'s retry policy:
    /// the commit's one retry owner. A wait's resolution and its due
    /// settlement retry under the wait policy, every other commit under the
    /// durable one.
    async fn replay<T, Fut>(
        &self,
        label: CommitLabel,
        deadline: Option<tokio::time::Instant>,
        attempt: impl FnMut() -> Fut,
    ) -> Result<T, DurableError>
    where
        Fut: std::future::Future<Output = Result<T, Attempt<DurableError>>>,
    {
        let policy = if label == CommitLabel::WAIT_RESOLVE || label == CommitLabel::WAIT_TIMEOUT {
            &self.pools.wait_retry
        } else {
            &self.pools.durable_retry
        };
        policy
            .run(deadline, Attempt::is_aborted, attempt)
            .await
            .map_err(Attempt::into_error)
    }

    /// [`open`](Self::open) for a replayable commit: its first statement
    /// also reads the transaction's id, by which a lost `COMMIT` is
    /// reconciled, and for an owner's commit `owner` runs the ownership
    /// fence, answering what it read under it (`None`: fenced out).
    async fn open_replayable(
        &self,
        label: CommitLabel,
        owner: Option<&ActorTx>,
    ) -> Result<(Tx, DurableInstant, Option<FenceRead>), Attempt<DurableError>> {
        tracing::trace!(label = label.as_str(), "durable postgres commit");
        let route = self.route(label.capacity());
        let mut locked = begin_durable(route.pool, &self.fence, route.prelude)
            .await
            .map_err(|error| attempt(store_failure(error)))?;
        let (recorded, now, xact, row) = match owner {
            Some(owner) => {
                let session = match owner.actor().kind() {
                    lash_durable::ActorKind::Session => Some(owner.actor().id()),
                    lash_durable::ActorKind::Process => None,
                };
                let row = crate::observed_sql::query(SQL.postgres.owner_envelope.sql())
                    .bind(owner.actor().as_str())
                    .bind(owner.epoch().0)
                    .bind(session)
                    .bind(owner.revision().0)
                    .fetch_one(crate::observed_sql::executor(locked.connection()))
                    .await
                    .map_err(|error| attempt(sqlx_failure(error)))?;
                let recorded: Option<i32> = get(&row, 0).map_err(Attempt::Failed)?;
                let now: i64 = get(&row, 1).map_err(Attempt::Failed)?;
                let xact: String = get(&row, 2).map_err(Attempt::Failed)?;
                (recorded, now, xact, Some(row))
            }
            None => {
                let (recorded, now, xact): (Option<i32>, i64, String) =
                    crate::observed_sql::query_as(SQL.postgres.fence_clock_and_xact.sql())
                        .fetch_one(crate::observed_sql::executor(locked.connection()))
                        .await
                        .map_err(|error| attempt(sqlx_failure(error)))?;
                (recorded, now, xact, None)
            }
        };
        let mut tx = locked
            .admit(&self.fence, recorded)
            .await
            .map_err(|error| attempt(store_failure(error)))?;
        tx.note_xact(XactId::new(xact));
        let now = self.instant_or(now).map_err(Attempt::Failed)?;
        let fence = match (owner, row) {
            (Some(owner), Some(row)) => {
                fenced_read(owner.actor(), &row).map_err(Attempt::Failed)?
            }
            _ => None,
        };
        Ok((tx, now, fence))
    }

    /// End a replayable commit's attempt: roll a refusal back, and commit a
    /// value, reconciling a lost `COMMIT` from the transaction's recorded
    /// outcome. Contention and a confirmed rollback of a lost `COMMIT` are
    /// aborted attempts; an unknown outcome is the operation's answer.
    async fn settle<T>(
        &self,
        label: CommitLabel,
        tx: Tx,
        outcome: Result<T, DurableError>,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<T, Attempt<DurableError>> {
        let value = match outcome {
            Ok(value) => value,
            Err(error) => {
                // A failed transaction is gone before any pause: rolled back
                // here, or by the server when its connection closes.
                let _ = tx.rollback().await;
                return Err(attempt(error));
            }
        };
        let settle = Settle {
            pool: self.route(label.capacity()).pool,
            retry: &self.pools.durable_retry,
            deadline,
            #[cfg(any(test, feature = "testing"))]
            fault: self.commit_fault.as_deref(),
        };
        match tx.commit_reconciled(&settle).await {
            Ok(()) => Ok(value),
            Err(Uncommitted::RolledBack(error)) => Err(attempt(sqlx_failure(error))),
            Err(Uncommitted::Lost(error)) => {
                Err(Attempt::Aborted(DurableError::Store(StoreFailure {
                    code: crate::support::storage_code(&error),
                    kind: StoreFailureKind::Unavailable,
                    message: format!(
                        "the connection was lost at COMMIT, which rolled back: {error}"
                    ),
                })))
            }
            Err(Uncommitted::Unknown { message, code }) => {
                Err(Attempt::Failed(DurableError::Store(StoreFailure {
                    code,
                    kind: StoreFailureKind::Unavailable,
                    message,
                })))
            }
        }
    }
}

/// Apply the owner's `write` after its ownership fence, which the commit's
/// first statement ran ([`super::PostgresDurableStatements::owner_envelope`]):
/// `fence` is what it read under the fence, `None` when the epoch was not
/// current. The roots the commit retired are held for retirement steps
/// (D-RET).
pub(super) async fn apply_owner(
    tx: &mut Tx,
    write: &ActorTx,
    riding: Option<&lash_durable::domain::graph_commit::CheckedGraph>,
    fence: Option<FenceRead>,
    now: DurableInstant,
    fleet: lash_core_store::store::FleetFormat,
    pools: &crate::host::RolePools,
) -> Result<ActorCommit, DurableError> {
    let actor = write.actor().as_str();
    let Some(fence) = fence else {
        return Err(fenced(tx, write.actor(), write.epoch(), write.revision()).await?);
    };
    PostgresDurableStore::lock_cross_target_heads(tx, write).await?;
    if write.domain().iter().any(|domain| {
        matches!(
            domain,
            DomainWrite::RegisterExecution(_)
                | DomainWrite::EndExecution(_)
                | DomainWrite::AdoptExecution(_)
                | DomainWrite::SeedResolution(_)
        )
    }) && fence.revision.0 - 1 != write.revision().0
    {
        return Err(DurableError::ResolutionRevisionConflict {
            actor: write.actor().clone(),
            expected: write.revision(),
            current: lash_durable::StateRevision(fence.revision.0 - 1),
        });
    }
    let publications: Vec<_> = write
        .domain()
        .iter()
        .flat_map(DomainWrite::publications)
        .collect();
    if write
        .domain()
        .iter()
        .any(|w| matches!(w, DomainWrite::Environment(_)))
        || !write.graph().is_empty()
        || !write.reads().is_empty()
        || !publications.is_empty()
        || riding.is_some()
    {
        let mut checked = lash_durable::domain::graph_commit::check_graph(
            write.graph(),
            write.reads(),
            &publications,
        )?;
        if let Some(riding) = riding {
            checked = pages::with_riding(checked, riding);
        }
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
        super::environment::validate_graph(tx, &checked, &publishing).await?;
        if let Some(budget) = write.budget() {
            lash_durable::domain::graph_commit::measure_owner_commit(write.graph(), write.domain())
                .and_then(|measured| budget.admit(&measured))?;
        }
        graph::validate_exact_reads(tx, write.exact_reads()).await?;
        graph::install(tx, &checked).await?;
    }
    let (process_view, locked) = super::process_view::refresh_preimage(tx, write).await?;
    let mut committing = Committing {
        process_views: lash_durable::domain::processes::ProcessPostimages::from_view(process_view),
        checkpoint_ref_chunk: pools.maintenance.checkpoint_ref_chunk as usize,
        actor: write.actor(),
        epoch: write.epoch(),
        now,
        fleet,
        retired: Retired::default(),
        configs: &pools.configs,
        ending: Vec::new(),
        locked: std::sync::Mutex::new(locked),
    };
    let ending = crate::obligation_ledger::lock_ending_executions_tx(tx, write.domain())
        .await
        .map_err(store_failure)?;
    // The artifact locks of every end the transaction runs are taken in
    // one sorted acquisition before any of them (FIG-7700): by its head
    // commit, which alone knows the frames it leaves, else now.
    if write
        .domain()
        .iter()
        .any(|domain| matches!(domain, DomainWrite::SessionCommit(_)))
    {
        committing.ending = ending;
    } else {
        crate::artifact_store::lock_ends_tx(
            tx,
            &ending
                .iter()
                .cloned()
                .map(lash_core_store::artifact_referrer::ArtifactReferrer::Execution)
                .collect::<Vec<_>>(),
            &ending.iter().collect::<Vec<_>>(),
            &[],
        )
        .await
        .map_err(store_failure)?;
    }
    let mut session_heads = Vec::new();
    for domain in write.domain() {
        committing.follow(domain);
        match domain {
            DomainWrite::SessionCommit(head) => {
                let receipt = super::turns::apply_session_commit(tx, &committing, head).await?;
                session_heads.push(lash_durable::CommittedSessionHead {
                    session: head.session.clone(),
                    receipt,
                });
            }
            domain => apply_domain(tx, &committing, domain).await?,
        }
        committing.process_views.applied(domain, write.epoch());
    }
    let process_views = committing.process_views.into_views();
    let mut root_leases = Vec::new();
    for request in write.root_leases() {
        root_leases.push(pages::acquire_in(tx, request.clone(), now).await?);
    }
    super::environment::finish(
        tx,
        &super::steps::Context {
            retired: &committing.retired,
            actor: committing.actor,
            now: committing.now,
        },
        write.domain(),
    )
    .await?;
    for lease in write.released_root_leases() {
        if let Some(root) = pages::release_in(tx, *lease).await? {
            committing.retired.stored(root);
        }
    }
    pages::hold_retired(tx, committing.retired, now)
        .await
        .map_err(sqlx_failure)?;
    if let Some(formats) = write.formats() {
        crate::observed_sql::query(SQL.actor.stamp_formats.sql())
            .bind(actor)
            .bind(formats.as_str())
            .execute(crate::observed_sql::executor(&mut ***tx))
            .await
            .map_err(sqlx_failure)?;
    }
    if write.requires().is_some() || write.domain().iter().any(super::executor::changes_debt) {
        packages::stamp_requires(tx, write.actor(), write.requires()).await?;
    }
    if let Some(through) = write.ack() {
        crate::observed_sql::query(SQL.postgres.ack_through.sql())
            .bind(actor)
            .bind(through.0)
            .execute(crate::observed_sql::executor(&mut ***tx))
            .await
            .map_err(sqlx_failure)?;
    }
    let state = match write.release() {
        None => ActorState::Owned,
        Some(Release::Parked) => {
            // A stop that reached the actor since the owner's read is not
            // lost to the park: the actor goes ready instead, its park kept,
            // and its claimer runs the stop (FIG-6099). A stop that commits
            // after this transaction's fence waits on the actor row it
            // locked, and its control wake readies the parked actor.
            let stopped = write.immediate_cancel_since_read(fence.turn_cancel.as_ref())
                || crate::observed_sql::query(SQL.park.stop_mail_after.sql())
                    .bind(actor)
                    .bind(write.seen().0)
                    .bind(lash_durable::domain::CANCEL_MAIL)
                    .bind(lash_durable::domain::SESSION_CLOSE_MAIL)
                    .fetch_optional(crate::observed_sql::executor(&mut ***tx))
                    .await
                    .map_err(sqlx_failure)?
                    .is_some();
            let stored: String = if stopped {
                crate::observed_sql::query_scalar(SQL.actor.release.sql())
                    .bind(actor)
                    .bind("ready")
                    .bind(Option::<i64>::None)
                    .bind(now.0)
                    .fetch_one(crate::observed_sql::executor(&mut ***tx))
                    .await
                    .map_err(sqlx_failure)?
            } else {
                crate::observed_sql::query_scalar(SQL.park.park.sql())
                    .bind(actor)
                    .fetch_one(crate::observed_sql::executor(&mut ***tx))
                    .await
                    .map_err(sqlx_failure)?
            };
            actor_state(&stored)?
        }
        Some(Release::Terminal) => {
            crate::observed_sql::query(SQL.mail.delete_all.sql())
                .bind(actor)
                .execute(crate::observed_sql::executor(&mut ***tx))
                .await
                .map_err(sqlx_failure)?;
            let stored: String = crate::observed_sql::query_scalar(SQL.actor.end.sql())
                .bind(actor)
                .fetch_one(crate::observed_sql::executor(&mut ***tx))
                .await
                .map_err(sqlx_failure)?;
            actor_state(&stored)?
        }
        Some(rest) => {
            let (state, due) = match rest {
                Release::Waiting { next_due } => ("waiting", next_due.map(|due| due.0)),
                Release::Ready => ("ready", None),
                _ => ("idle", None),
            };
            let stored: String = crate::observed_sql::query_scalar(SQL.actor.release.sql())
                .bind(actor)
                .bind(state)
                .bind(due)
                .bind(now.0)
                .fetch_one(crate::observed_sql::executor(&mut ***tx))
                .await
                .map_err(sqlx_failure)?;
            if rest == Release::Ready && !write.advances() {
                crate::observed_sql::query(SQL.actor.unadvanced.sql())
                    .bind(actor)
                    .execute(crate::observed_sql::executor(&mut ***tx))
                    .await
                    .map_err(sqlx_failure)?;
            }
            actor_state(&stored)?
        }
    };
    Ok(ActorCommit {
        process_views,
        session_heads,
        root_leases,
        revision: fence.revision,
        state,
        turn_cancel: fence.turn_cancel,
    })
}

/// What an owner commit's fence read: the state revision it bumped and,
/// for a session actor, its unfinished turn's accepted cancel request.
pub(super) struct FenceRead {
    pub(super) revision: lash_durable::StateRevision,
    pub(super) turn_cancel: Option<lash_durable::domain::TurnCancelRequest>,
}

/// What [`super::PostgresDurableStatements::owner_envelope`]'s `row` read under
/// `actor`'s fence: `None` when the fence did not match.
fn fenced_read(
    actor: &lash_durable::ActorKey,
    row: &sqlx::postgres::PgRow,
) -> Result<Option<FenceRead>, DurableError> {
    let Some(revision) = get::<Option<i64>>(row, 3)? else {
        return Ok(None);
    };
    let turn_cancel = match (actor.kind(), get::<Option<String>>(row, 4)?) {
        (lash_durable::ActorKind::Session, Some(run)) => {
            let session = lash_kernel_value::identity::SessionId::try_from(actor.id().to_owned())
                .map_err(|_| super::corrupt("session id", actor.id()))?;
            let run = lash_vocabulary::identity::TurnId::try_from(run.clone())
                .map_err(|_| super::corrupt("turn id", &run))?;
            let stored = (
                get(row, 5)?,
                get(row, 6)?,
                get(row, 7)?,
                get(row, 8)?,
                get(row, 9)?,
            );
            Some(super::turns::decode_cancel(&session, &run, stored)?)
        }
        _ => None,
    };
    Ok(Some(FenceRead {
        revision: lash_durable::StateRevision(revision),
        turn_cancel,
    }))
}
