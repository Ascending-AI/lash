//! The test writer takes one unowned row, then uses the ordinary epoch fence.
use super::*;
use lash_store_sql::durable::TestActorStatements;

static TEST_SQL: LazyLock<TestActorStatements> =
    LazyLock::new(|| TestActorStatements::render(crate::schema_layout::MAIN));

impl SqliteDurableStore {
    pub(super) async fn acquire_test_writer(
        &self,
        node: &NodeLease,
        actor: &ActorKey,
        epoch: Epoch,
    ) -> Result<Epoch, DurableError> {
        let owner = node.owner.clone();
        let actor = actor.clone();
        self.write(CommitLabel::CLAIM, move |tx, _now| {
            if !node_live(tx, &owner)? {
                return refuse(DurableError::NodeLeaseLost { node: owner.node });
            }
            let acquired = tx
                .prepare_cached(TEST_SQL.acquire_idle.sql())?
                .query_row(
                    rusqlite::params![
                        actor.as_str(),
                        epoch.0,
                        owner.node.as_str(),
                        owner.boot.as_str()
                    ],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            if let Some(acquired) = acquired {
                return commit(Epoch(acquired));
            }
            let current = tx
                .prepare_cached(SQL.actor.epoch_of.sql())?
                .query_row([actor.as_str()], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .optional()?;
            refuse(match current {
                None => DurableError::MailRefused(MailRefusal::UnknownActor(actor)),
                Some((_, state)) if state == ActorState::Terminal.as_str() => {
                    DurableError::MailRefused(MailRefusal::ActorTerminal(actor))
                }
                Some((current, _)) => DurableError::OwnershipLost(Fenced {
                    actor,
                    held: epoch,
                    current: Some(Epoch(current)),
                }),
            })
        })
        .await
    }
}
