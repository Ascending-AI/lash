//! The test writer takes one unowned row, then uses the ordinary epoch fence.
use super::*;
use lash_store_sql::durable::TestActorStatements;

static TEST_SQL: LazyLock<TestActorStatements> =
    LazyLock::new(|| TestActorStatements::render(Dialect::postgres()));

impl PostgresDurableStore {
    pub(super) async fn acquire_test_writer(
        &self,
        node: &NodeLease,
        actor: &ActorKey,
        epoch: Epoch,
    ) -> Result<Epoch, DurableError> {
        self.within(CommitCapacity::Scheduler, async {
            let (mut tx, _now) = self.open(CommitLabel::CLAIM).await?;
            let outcome = async {
                if node_draining(&mut tx, &node.owner).await?.is_none() {
                    return Err(DurableError::NodeLeaseLost {
                        node: node.owner.node.clone(),
                    });
                }
                let acquired = crate::observed_sql::query(TEST_SQL.acquire_idle.sql())
                    .bind(actor.as_str())
                    .bind(epoch.0)
                    .bind(node.owner.node.as_str())
                    .bind(node.owner.boot.as_str())
                    .fetch_optional(crate::observed_sql::executor(&mut **tx))
                    .await
                    .map_err(sqlx_failure)?;
                if let Some(row) = acquired {
                    return Ok(Epoch(get(&row, 0)?));
                }
                let current = crate::observed_sql::query(SQL.actor.epoch_of.sql())
                    .bind(actor.as_str())
                    .fetch_optional(crate::observed_sql::executor(&mut **tx))
                    .await
                    .map_err(sqlx_failure)?;
                Err(match current {
                    None => DurableError::MailRefused(MailRefusal::UnknownActor(actor.clone())),
                    Some(row) if get::<String>(&row, 1)? == ActorState::Terminal.as_str() => {
                        DurableError::MailRefused(MailRefusal::ActorTerminal(actor.clone()))
                    }
                    Some(row) => DurableError::OwnershipLost(Fenced {
                        actor: actor.clone(),
                        held: epoch,
                        current: Some(Epoch(get(&row, 0)?)),
                    }),
                })
            }
            .await;
            finish(tx, outcome).await
        })
        .await
    }
}
