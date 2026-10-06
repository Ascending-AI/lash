//! The durability engine over PostgreSQL: [`PostgresDurableStore`] and the
//! statements that fork from the neutral set in `lash_store_sql::durable`.
//!
//! This is the PostgreSQL engine module: apart from `schema.sql` and the
//! artifacts generated from it, no other file in this crate may name the
//! engine's tables (`scripts/check-durable-sql.py`).
//!
//! Every write is one guarded transaction: the writer fence first, then the
//! database clock read once, then the engine's statements. A claim locks its
//! node row `FOR SHARE` and its actors `FOR UPDATE SKIP LOCKED`, so a
//! concurrent reap waits for it and concurrent claimers never take one actor
//! twice; an owner commit's fence is a conditional `UPDATE` of the actor row,
//! which a concurrent claim or reap either precedes or waits behind.

use std::sync::LazyLock;

use lash_durable::{
    ActorCommit, ActorKey, ActorKind, ActorSnapshot, ActorState, ActorTx, BootId, ClaimCause,
    Claimed, CommitLabel, DurableError, DurableInstant, DurableStore, Epoch, Fenced, FormatSet,
    HeartbeatOutcome, Mail, MailCommit, MailKind, MailRefusal, MailSeq, MailTx, MailWrite, NodeId,
    NodeLease, NodeSpec, OpenedActor, Owner, Reaped, Release, StateRevision, StoreFailure,
    StoreFailureKind, Woken,
};
use lash_store_sql::Dialect;
use lash_store_sql::durable::{ActorStatements, MailStatements, NodeStatements};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, PgPool, Row};

use crate::StoreError;
use crate::guarded_tx::{GuardedTx, WriterFence, begin_guarded};
use crate::support::store_sqlx_error;

lash_store_sql::statements! {
    /// The engine statements only PostgreSQL issues.
    pub(crate) struct PostgresDurableStatements @ "durable_postgres" {
        /// Boot `?2` of node `?1`'s lease, locked against a concurrent reap
        /// for the rest of the transaction.
        node_live = "SELECT 1 FROM nodes WHERE node_id = ?1 AND boot_id = ?2 FOR SHARE";

        /// Give up to `?5` actors claimable at `?1` in a format set of `?4`
        /// to boot `?3` of node `?2`, oldest first, bumping each epoch;
        /// returns each with the state that made it claimable.
        claim = "WITH c AS (
                 SELECT actor_key, state FROM actors
                 WHERE ((state = 'ready' AND ready_at_ms <= ?1)
                     OR (state = 'waiting' AND next_due_ms <= ?1))
                   AND formats = ANY(?4)
                 ORDER BY COALESCE(ready_at_ms, next_due_ms), actor_key
                 LIMIT ?5
                 FOR UPDATE SKIP LOCKED
             )
             UPDATE actors AS a
             SET state = 'owned', epoch = a.epoch + 1, owner_node = ?2, owner_boot = ?3,
                 ready_at_ms = NULL, next_due_ms = NULL
             FROM c
             WHERE a.actor_key = c.actor_key
             RETURNING a.actor_key, a.epoch, c.state";
    }
}

struct Sql {
    node: NodeStatements,
    actor: ActorStatements,
    mail: MailStatements,
    postgres: PostgresDurableStatements,
}

static SQL: LazyLock<Sql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    Sql {
        node: NodeStatements::render(dialect),
        actor: ActorStatements::render(dialect),
        mail: MailStatements::render(dialect),
        postgres: PostgresDurableStatements::render(dialect),
    }
});

/// The [`DurableStore`] over one PostgreSQL catalog.
#[derive(Clone)]
pub struct PostgresDurableStore {
    pool: PgPool,
    fence: WriterFence,
    /// The clock a test stands in for the database's.
    #[cfg(any(test, feature = "testing"))]
    clock: Option<std::sync::Arc<dyn lash_core_execution::Clock>>,
}

impl std::fmt::Debug for PostgresDurableStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresDurableStore")
            .finish_non_exhaustive()
    }
}

type Tx = GuardedTx<'static>;

fn store_failure(error: StoreError) -> DurableError {
    let kind = match &error {
        StoreError::Contended => StoreFailureKind::Contended,
        StoreError::WriterFenced { .. } => StoreFailureKind::WriterRetired,
        _ => StoreFailureKind::Unavailable,
    };
    DurableError::Store(StoreFailure {
        kind,
        message: error.to_string(),
    })
}

fn sqlx_failure(error: sqlx::Error) -> DurableError {
    match error {
        sqlx::Error::ColumnDecode { .. } | sqlx::Error::Decode(_) => {
            DurableError::Store(StoreFailure {
                kind: StoreFailureKind::Corrupt,
                message: error.to_string(),
            })
        }
        error => store_failure(store_sqlx_error(error)),
    }
}

fn corrupt(what: &str, value: &str) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Corrupt,
        message: format!("stored {what} `{value}` does not decode"),
    })
}

fn actor_key(stored: &str) -> Result<ActorKey, DurableError> {
    ActorKey::parse(stored).map_err(|_| corrupt("actor key", stored))
}

fn actor_state(stored: &str) -> Result<ActorState, DurableError> {
    ActorState::parse(stored).ok_or_else(|| corrupt("actor state", stored))
}

fn owner(node: Option<String>, boot: Option<String>) -> Option<Owner> {
    Some(Owner {
        node: NodeId::new(node?),
        boot: BootId::new(boot?),
    })
}

fn get<'r, T>(row: &'r PgRow, index: usize) -> Result<T, DurableError>
where
    T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get(index).map_err(sqlx_failure)
}

/// Commit `tx` when `outcome` is a value, roll it back when it is a refusal.
async fn finish<T>(tx: Tx, outcome: Result<T, DurableError>) -> Result<T, DurableError> {
    match outcome {
        Ok(value) => {
            tx.commit().await.map_err(sqlx_failure)?;
            Ok(value)
        }
        Err(error) => {
            tx.rollback().await.map_err(sqlx_failure)?;
            Err(error)
        }
    }
}

impl PostgresDurableStore {
    pub(crate) fn new(pool: PgPool, fence: WriterFence) -> Self {
        Self {
            pool,
            fence,
            #[cfg(any(test, feature = "testing"))]
            clock: None,
        }
    }

    /// Read `clock` where the database clock would be read: a test moves
    /// time instead of waiting for it.
    #[cfg(any(test, feature = "testing"))]
    pub fn with_clock_for_testing(
        mut self,
        clock: std::sync::Arc<dyn lash_core_execution::Clock>,
    ) -> Self {
        self.clock = Some(clock);
        self
    }

    fn injected_instant(&self) -> Option<DurableInstant> {
        #[cfg(any(test, feature = "testing"))]
        if let Some(clock) = &self.clock {
            return Some(DurableInstant(
                i64::try_from(clock.timestamp_ms()).unwrap_or(i64::MAX),
            ));
        }
        None
    }

    async fn instant_on(
        &self,
        connection: &mut PgConnection,
    ) -> Result<DurableInstant, DurableError> {
        if let Some(now) = self.injected_instant() {
            return Ok(now);
        }
        let now: i64 = sqlx::query_scalar(
            crate::connection_sql::connection_sql()
                .select_statement_epoch_ms
                .sql(),
        )
        .fetch_one(connection)
        .await
        .map_err(sqlx_failure)?;
        Ok(DurableInstant(now))
    }

    /// A guarded transaction and the instant it runs at.
    async fn open(&self, label: CommitLabel) -> Result<(Tx, DurableInstant), DurableError> {
        tracing::trace!(label = label.as_str(), "durable postgres commit");
        let mut tx: Tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(store_failure)?;
        let now = self.instant_on(&mut tx).await?;
        Ok((tx, now))
    }
}

async fn node_live(tx: &mut PgConnection, node: &Owner) -> Result<bool, DurableError> {
    Ok(sqlx::query(SQL.postgres.node_live.sql())
        .bind(node.node.as_str())
        .bind(node.boot.as_str())
        .fetch_optional(tx)
        .await
        .map_err(sqlx_failure)?
        .is_some())
}

async fn release_owned_by(
    tx: &mut PgConnection,
    owner: &Owner,
    now: DurableInstant,
) -> Result<Vec<(ActorKey, Epoch)>, DurableError> {
    let rows = sqlx::query(SQL.actor.release_owned_by.sql())
        .bind(owner.node.as_str())
        .bind(owner.boot.as_str())
        .bind(now.0)
        .fetch_all(tx)
        .await
        .map_err(sqlx_failure)?;
    rows.iter()
        .map(|row| Ok((actor_key(&get::<String>(row, 0)?)?, Epoch(get(row, 1)?))))
        .collect()
}

async fn fenced(
    tx: &mut PgConnection,
    actor: &ActorKey,
    held: Epoch,
) -> Result<DurableError, DurableError> {
    let current: Option<i64> = sqlx::query_scalar(SQL.actor.epoch_of.sql())
        .bind(actor.as_str())
        .fetch_optional(tx)
        .await
        .map_err(sqlx_failure)?;
    Ok(DurableError::OwnershipLost(Fenced {
        actor: actor.clone(),
        held,
        current: current.map(Epoch),
    }))
}

async fn apply_owner(
    tx: &mut PgConnection,
    write: &ActorTx,
    now: DurableInstant,
) -> Result<ActorCommit, DurableError> {
    let actor = write.actor().as_str();
    let fence: Option<i64> = sqlx::query_scalar(SQL.actor.fence.sql())
        .bind(actor)
        .bind(write.epoch().0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(sqlx_failure)?;
    let Some(revision) = fence else {
        return Err(fenced(tx, write.actor(), write.epoch()).await?);
    };
    if let Some(through) = write.ack() {
        for statement in [&SQL.actor.ack, &SQL.mail.delete_through] {
            sqlx::query(statement.sql())
                .bind(actor)
                .bind(through.0)
                .execute(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
        }
    }
    let state = match write.release() {
        None => ActorState::Owned,
        Some(Release::Terminal) => {
            sqlx::query(SQL.mail.delete_all.sql())
                .bind(actor)
                .execute(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            let stored: String = sqlx::query_scalar(SQL.actor.end.sql())
                .bind(actor)
                .fetch_one(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            actor_state(&stored)?
        }
        Some(rest) => {
            let (state, due) = match rest {
                Release::Waiting { next_due } => ("waiting", next_due.map(|due| due.0)),
                _ => ("idle", None),
            };
            let stored: String = sqlx::query_scalar(SQL.actor.release.sql())
                .bind(actor)
                .bind(state)
                .bind(due)
                .bind(now.0)
                .fetch_one(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            actor_state(&stored)?
        }
    };
    Ok(ActorCommit {
        revision: StateRevision(revision),
        state,
    })
}

fn note_woken(woken: &mut Vec<Woken>, entry: Woken) {
    match woken.iter_mut().find(|seen| seen.actor == entry.actor) {
        Some(seen) => *seen = entry,
        None => woken.push(entry),
    }
}

async fn apply_mail(
    tx: &mut PgConnection,
    writes: &MailTx,
    now: DurableInstant,
) -> Result<MailCommit, DurableError> {
    let mut receipt = MailCommit::default();
    for write in writes.writes() {
        match write {
            MailWrite::CreateActor { actor, formats } => {
                let created = sqlx::query(SQL.actor.create.sql())
                    .bind(actor.as_str())
                    .bind(actor.kind().as_str())
                    .bind(formats.as_str())
                    .bind(now.0)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(sqlx_failure)?;
                if created.is_none() {
                    return Err(DurableError::MailRefused(MailRefusal::ActorExists(
                        actor.clone(),
                    )));
                }
                note_woken(
                    &mut receipt.woken,
                    Woken {
                        actor: actor.clone(),
                        state: ActorState::Ready,
                        owner: None,
                    },
                );
            }
            MailWrite::Append { actor, .. } | MailWrite::Wake { actor } => {
                let woke = sqlx::query(SQL.actor.wake.sql())
                    .bind(actor.as_str())
                    .bind(now.0)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(sqlx_failure)?;
                let Some(row) = woke else {
                    let state: Option<(i64, String)> = sqlx::query_as(SQL.actor.epoch_of.sql())
                        .bind(actor.as_str())
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(sqlx_failure)?;
                    return Err(DurableError::MailRefused(match state {
                        None => MailRefusal::UnknownActor(actor.clone()),
                        Some(_) => MailRefusal::ActorTerminal(actor.clone()),
                    }));
                };
                let seq: i64 = get(&row, 0)?;
                if let MailWrite::Append { kind, body, .. } = write {
                    sqlx::query(SQL.mail.append.sql())
                        .bind(actor.as_str())
                        .bind(seq)
                        .bind(kind.as_str())
                        .bind(body)
                        .bind(now.0)
                        .execute(&mut *tx)
                        .await
                        .map_err(sqlx_failure)?;
                    receipt.appended.push((actor.clone(), MailSeq(seq)));
                }
                note_woken(
                    &mut receipt.woken,
                    Woken {
                        actor: actor.clone(),
                        state: actor_state(&get::<String>(&row, 1)?)?,
                        owner: owner(get(&row, 2)?, get(&row, 3)?),
                    },
                );
            }
        }
    }
    Ok(receipt)
}

#[async_trait::async_trait]
impl DurableStore for PostgresDurableStore {
    async fn now(&self) -> Result<DurableInstant, DurableError> {
        let mut connection = self.pool.acquire().await.map_err(sqlx_failure)?;
        self.instant_on(&mut connection).await
    }

    async fn register_node(&self, spec: &NodeSpec) -> Result<NodeLease, DurableError> {
        let lease_owner = Owner {
            node: spec.node.clone(),
            boot: BootId::new(uuid::Uuid::new_v4().to_string()),
        };
        let formats: Vec<&str> = spec.decodes.iter().map(FormatSet::as_str).collect();
        let formats = serde_json::Value::from(formats).to_string();
        let (mut tx, now) = self.open(CommitLabel::NODE_REGISTER).await?;
        let outcome = async {
            let boots: Vec<String> = sqlx::query_scalar(SQL.node.delete_boots.sql())
                .bind(spec.node.as_str())
                .fetch_all(&mut **tx)
                .await
                .map_err(sqlx_failure)?;
            for boot in boots {
                let earlier = Owner {
                    node: spec.node.clone(),
                    boot: BootId::new(boot),
                };
                release_owned_by(&mut tx, &earlier, now).await?;
            }
            let expires_at = now.after_millis(spec.ttl_millis);
            sqlx::query(SQL.node.insert.sql())
                .bind(lease_owner.node.as_str())
                .bind(lease_owner.boot.as_str())
                .bind(&formats)
                .bind(now.0)
                .bind(expires_at.0)
                .execute(&mut **tx)
                .await
                .map_err(sqlx_failure)?;
            Ok(NodeLease {
                owner: lease_owner,
                decodes: spec.decodes.clone(),
                ttl_millis: spec.ttl_millis,
                expires_at,
            })
        }
        .await;
        finish(tx, outcome).await
    }

    async fn heartbeat(&self, node: &NodeLease) -> Result<HeartbeatOutcome, DurableError> {
        let (mut tx, now) = self.open(CommitLabel::HEARTBEAT).await?;
        let outcome = sqlx::query_scalar::<_, i64>(SQL.node.renew.sql())
            .bind(node.owner.node.as_str())
            .bind(node.owner.boot.as_str())
            .bind(now.after_millis(node.ttl_millis).0)
            .fetch_optional(&mut **tx)
            .await
            .map_err(sqlx_failure)
            .map(|renewed| match renewed {
                Some(expires_at) => HeartbeatOutcome::Renewed {
                    expires_at: DurableInstant(expires_at),
                },
                None => HeartbeatOutcome::Reaped,
            });
        finish(tx, outcome).await
    }

    async fn reap(&self, reaper: &NodeLease) -> Result<Vec<Reaped>, DurableError> {
        let (mut tx, now) = self.open(CommitLabel::REAP).await?;
        let outcome = async {
            if !node_live(&mut tx, &reaper.owner).await? {
                return Err(DurableError::NodeLeaseLost {
                    node: reaper.owner.node.clone(),
                });
            }
            let dead: Vec<(String, String)> = sqlx::query_as(SQL.node.delete_expired.sql())
                .bind(now.0)
                .fetch_all(&mut **tx)
                .await
                .map_err(sqlx_failure)?;
            let mut reaped = Vec::new();
            for (node, boot) in dead {
                let from = Owner {
                    node: NodeId::new(node),
                    boot: BootId::new(boot),
                };
                for (actor, epoch) in release_owned_by(&mut tx, &from, now).await? {
                    reaped.push(Reaped {
                        actor,
                        from: from.clone(),
                        epoch,
                    });
                }
            }
            Ok(reaped)
        }
        .await;
        finish(tx, outcome).await
    }

    async fn release_node(&self, node: &NodeLease) -> Result<Vec<ActorKey>, DurableError> {
        let (mut tx, now) = self.open(CommitLabel::NODE_RELEASE).await?;
        let outcome = async {
            let deleted = sqlx::query(SQL.node.delete_boot.sql())
                .bind(node.owner.node.as_str())
                .bind(node.owner.boot.as_str())
                .fetch_optional(&mut **tx)
                .await
                .map_err(sqlx_failure)?;
            if deleted.is_none() {
                return Err(DurableError::NodeLeaseLost {
                    node: node.owner.node.clone(),
                });
            }
            Ok(release_owned_by(&mut tx, &node.owner, now)
                .await?
                .into_iter()
                .map(|(actor, _)| actor)
                .collect())
        }
        .await;
        finish(tx, outcome).await
    }

    async fn claim(&self, node: &NodeLease, limit: usize) -> Result<Vec<Claimed>, DurableError> {
        let formats: Vec<String> = node
            .decodes
            .iter()
            .map(|formats| formats.as_str().to_owned())
            .collect();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let (mut tx, now) = self.open(CommitLabel::CLAIM).await?;
        let outcome = async {
            if !node_live(&mut tx, &node.owner).await? {
                return Err(DurableError::NodeLeaseLost {
                    node: node.owner.node.clone(),
                });
            }
            let rows = sqlx::query(SQL.postgres.claim.sql())
                .bind(now.0)
                .bind(node.owner.node.as_str())
                .bind(node.owner.boot.as_str())
                .bind(&formats)
                .bind(limit)
                .fetch_all(&mut **tx)
                .await
                .map_err(sqlx_failure)?;
            let mut claimed = rows
                .iter()
                .map(|row| {
                    Ok(Claimed {
                        actor: actor_key(&get::<String>(row, 0)?)?,
                        epoch: Epoch(get(row, 1)?),
                        cause: if get::<String>(row, 2)? == ActorState::Waiting.as_str() {
                            ClaimCause::Due
                        } else {
                            ClaimCause::Ready
                        },
                    })
                })
                .collect::<Result<Vec<_>, DurableError>>()?;
            claimed.sort_by(|left, right| left.actor.cmp(&right.actor));
            Ok(claimed)
        }
        .await;
        finish(tx, outcome).await
    }

    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
        let rows: Vec<(String, i64)> = sqlx::query_as(SQL.actor.owned_by.sql())
            .bind(node.owner.node.as_str())
            .bind(node.owner.boot.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(sqlx_failure)?;
        rows.into_iter()
            .map(|(key, epoch)| {
                Ok(Claimed {
                    actor: actor_key(&key)?,
                    epoch: Epoch(epoch),
                    cause: ClaimCause::Adopted,
                })
            })
            .collect()
    }

    async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
        let rows = sqlx::query(SQL.actor.open.sql())
            .bind(actor.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(sqlx_failure)?;
        let Some(first) = rows.first() else {
            return Err(DurableError::OwnershipLost(Fenced {
                actor: actor.clone(),
                held: epoch,
                current: None,
            }));
        };
        let current = Epoch(get(first, 0)?);
        if current != epoch || get::<String>(first, 1)? != ActorState::Owned.as_str() {
            return Err(DurableError::OwnershipLost(Fenced {
                actor: actor.clone(),
                held: epoch,
                current: Some(current),
            }));
        }
        let mut opened = OpenedActor {
            actor: actor.clone(),
            epoch,
            revision: StateRevision(get(first, 2)?),
            acked: MailSeq(get(first, 3)?),
            seen: MailSeq(get(first, 4)?),
            mail: Vec::new(),
        };
        for row in &rows {
            if let Some(seq) = get::<Option<i64>>(row, 5)? {
                opened.mail.push(Mail {
                    seq: MailSeq(seq),
                    kind: MailKind::new(get::<String>(row, 6)?),
                    body: get(row, 7)?,
                    appended_at: DurableInstant(get(row, 8)?),
                });
            }
        }
        Ok(ActorTx::opened(opened))
    }

    async fn commit(&self, tx: ActorTx, label: CommitLabel) -> Result<ActorCommit, DurableError> {
        if tx.ack().is_some_and(|through| through > tx.seen()) {
            return Err(DurableError::AckBeyondRead {
                actor: tx.actor().clone(),
            });
        }
        let (mut guarded, now) = self.open(label).await?;
        let outcome = apply_owner(&mut guarded, &tx, now).await;
        finish(guarded, outcome).await
    }

    async fn commit_mail(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        let (mut guarded, now) = self.open(label).await?;
        let outcome = apply_mail(&mut guarded, &tx, now).await;
        finish(guarded, outcome).await
    }

    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
        let Some(row) = sqlx::query(SQL.actor.snapshot.sql())
            .bind(actor.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(sqlx_failure)?
        else {
            return Ok(None);
        };
        let kind: String = get(&row, 0)?;
        let mail_seq: i64 = get(&row, 5)?;
        let acked_seq: i64 = get(&row, 6)?;
        Ok(Some(ActorSnapshot {
            actor: actor.clone(),
            kind: ActorKind::parse(&kind).ok_or_else(|| corrupt("actor kind", &kind))?,
            state: actor_state(&get::<String>(&row, 1)?)?,
            epoch: Epoch(get(&row, 2)?),
            owner: owner(get(&row, 3)?, get(&row, 4)?),
            has_mail: mail_seq > acked_seq,
            next_due: get::<Option<i64>>(&row, 7)?.map(DurableInstant),
            revision: StateRevision(get(&row, 8)?),
            formats: FormatSet::new(get::<String>(&row, 9)?),
            pending_mail: u64::try_from(get::<i64>(&row, 10)?).unwrap_or_default(),
        }))
    }
}

#[cfg(test)]
#[path = "durable_tests.rs"]
mod tests;
