//! Guarded node lease renewal and expiry maintenance.

use std::sync::LazyLock;

use lash_durable::{
    BootId, CommitCapacity, CommitLabel, DurableError, DurableInstant, HeartbeatOutcome, NodeId,
    NodeLease, Owner, Reaped,
};

use super::{
    PostgresDurableStore, SQL, finish, node_live, release_owned_by, sqlx_failure, store_failure,
};

/// The server function that renews a node's lease in one request
/// (FIG-7049), generated from this build's statements and declared in
/// `schema.sql`.
pub(super) const FUNCTION: &str = "lash_node_renew";

/// The renewal's call.
static CALL: LazyLock<String> =
    LazyLock::new(|| format!("SELECT * FROM {FUNCTION}($1, $2, $3, $4, $5, $6, $7, $8)"));

impl PostgresDurableStore {
    /// Reaping keeps its fleet and live-node checks even when no node expired.
    /// An empty DELETE is not a durable transition and needs no COMMIT.
    pub(super) async fn reap_expired_nodes(
        &self,
        reaper: &NodeLease,
    ) -> Result<Vec<Reaped>, DurableError> {
        self.within(CommitCapacity::Critical, async {
            crate::observed_sql::measure(&self.observer, CommitLabel::REAP, 0, async {
                let (mut tx, now) = self.open(CommitLabel::REAP).await?;
                let outcome = async {
                    if !node_live(&mut tx, &reaper.owner).await? {
                        return Err(DurableError::NodeLeaseLost {
                            node: reaper.owner.node.clone(),
                        });
                    }
                    let dead: Vec<(String, String)> =
                        crate::observed_sql::query_as(SQL.node.delete_expired.sql())
                            .bind(now.0)
                            .bind(reaper.owner.node.as_str())
                            .bind(reaper.owner.boot.as_str())
                            .fetch_all(crate::observed_sql::executor(&mut **tx))
                            .await
                            .map_err(sqlx_failure)?;
                    if dead.is_empty() {
                        return Ok(None);
                    }
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
                    Ok(Some(reaped))
                }
                .await;
                match outcome {
                    Ok(None) => {
                        tx.rollback().await.map_err(sqlx_failure)?;
                        Ok(Vec::new())
                    }
                    outcome => finish(tx, outcome).await.map(Option::unwrap_or_default),
                }
            })
            .await
        })
        .await
    }

    /// Renew on the reserved capacity, admitting the epoch the routine read
    /// after its fleet lock before answering a renewed expiry.
    pub(super) async fn renew_node(
        &self,
        node: &NodeLease,
    ) -> Result<HeartbeatOutcome, DurableError> {
        self.within(CommitCapacity::Renewal, async {
            crate::observed_sql::measure(&self.observer, CommitLabel::HEARTBEAT, 0, async {
                let route = self.route(CommitCapacity::Renewal);
                let mut connection = crate::observed_sql::checkout(route.pool)
                    .await
                    .map_err(sqlx_failure)?;
                crate::observed_sql::transaction_started();
                let writable = self.fence.writable();
                let bound = |version: u32| i32::try_from(version).unwrap_or(i32::MAX);
                let (lock_timeout, statement_ms) = route.prelude.call_guards();
                let injected = self.injected_instant().transpose()?;
                let (recorded, expires_at): (Option<i32>, Option<i64>) =
                    crate::observed_sql::query_as(&CALL)
                        .bind(node.owner.node.as_str())
                        .bind(node.owner.boot.as_str())
                        .bind(node.ttl_millis)
                        .bind(bound(writable.min()))
                        .bind(bound(writable.max()))
                        .bind(lock_timeout)
                        .bind(statement_ms)
                        .bind(injected.map(|now| now.0))
                        .fetch_one(crate::observed_sql::executor(&mut *connection))
                        .await
                        .map_err(sqlx_failure)?;
                // An epoch the fence refuses renewed nothing.
                self.fence.admit(recorded).await.map_err(store_failure)?;
                Ok(match expires_at {
                    Some(expires_at) => HeartbeatOutcome::Renewed {
                        expires_at: DurableInstant(expires_at),
                    },
                    None => HeartbeatOutcome::Reaped,
                })
            })
            .await
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::ServerTimeout;
    use lash_durable::{DurableStore as _, FormatSet, NodeId, NodeSpec};
    use std::time::Duration;

    /// An imported pool has no startup statement guard. A renewal that exceeds
    /// the role's statement budget must roll back, never report an extended lease.
    #[tokio::test]
    async fn an_imported_renewal_cannot_commit_past_its_statement_budget() {
        let database_url = crate::testing::required_database_url();
        let database = crate::testing::IsolatedDatabase::create(&database_url).await;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(8)
            .connect(database.url())
            .await
            .expect("imported pool");
        let mut config = crate::testing::fixture_config();
        config.roles.max_store_operations = 8;
        config.guards.renewal.lock = ServerTimeout::Limit(Duration::from_millis(20));
        config.guards.renewal.statement = ServerTimeout::Limit(Duration::from_millis(50));
        let storage = crate::testing::from_pool(pool, &config)
            .await
            .expect("storage");
        let store = storage.durable_store();
        let lease = store
            .register_node(&NodeSpec {
                node: NodeId::new("statement-budget"),
                decodes: vec![FormatSet::new("law-formats")],
                serves: Vec::new(),
                capabilities: Default::default(),
                ttl_millis: 15_000,
            })
            .await
            .expect("lease");
        let before: i64 = sqlx::query_scalar(
            "SELECT heartbeat_expires_at_ms FROM lash_nodes WHERE node_id = 'statement-budget'",
        )
        .fetch_one(storage.pool())
        .await
        .expect("original expiry");
        sqlx::raw_sql("CREATE FUNCTION slow_renewal() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(0.1); RETURN NEW; END $$;
            CREATE TRIGGER slow_renewal BEFORE UPDATE ON lash_nodes FOR EACH ROW EXECUTE FUNCTION slow_renewal();")
            .execute(storage.pool()).await.expect("delay renewal past its budget");
        let answer = store.heartbeat(&lease).await;
        assert!(
            matches!(&answer, Err(DurableError::Store(failure)) if failure.message.contains("statement timeout")),
            "expired statement budget did not refuse the renewal: {answer:?}"
        );
        let after: i64 = sqlx::query_scalar(
            "SELECT heartbeat_expires_at_ms FROM lash_nodes WHERE node_id = 'statement-budget'",
        )
        .fetch_one(storage.pool())
        .await
        .expect("expiry after refusal");
        assert_eq!(before, after, "a refused renewal changed the lease");
    }
}
