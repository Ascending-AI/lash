//! The advisory scheduling reader; one SELECT, no writer gate.
use super::*;
use lash_durable::{ClaimActorFacts, ClaimFacts, ClaimNodeFacts};
use lash_store_sql::durable::ClaimFactsStatements;

static SQL: LazyLock<ClaimFactsStatements> =
    LazyLock::new(|| ClaimFactsStatements::render(crate::schema_layout::MAIN));

impl SqliteDurableStore {
    pub(super) async fn read_claim_facts(
        &self,
        node: Option<&Owner>,
        actor: &ActorKey,
    ) -> Result<ClaimFacts, DurableError> {
        let node = node.cloned();
        let actor = actor.clone();
        let clock = Arc::clone(&self.clock);
        self.read(move |tx| {
            let observed_at = integer(clock.timestamp_ms())
                .map(DurableInstant)
                .map_err(store_failure);
            tx.prepare_cached(SQL.read.sql())?.query_row(
                rusqlite::params![
                    node.as_ref().map(|node| node.node.as_str()),
                    actor.as_str(),
                    lash_durable::domain::CANCEL_MAIL,
                    lash_durable::domain::SESSION_CLOSE_MAIL
                ],
                |row| {
                    let boot: Option<String> = row.get(0)?;
                    let draining: bool = row.get::<_, Option<bool>>(1)?.unwrap_or(false);
                    let state: Option<String> = row.get(2)?;
                    let stored: Option<String> = row.get(8)?;
                    let result = (|| {
                        Ok(ClaimFacts {
                            observed_at: observed_at?,
                            node: boot.map(|boot| ClaimNodeFacts {
                                owner: Owner {
                                    node: NodeId::new(
                                        node.as_ref().map_or("", |node| node.node.as_str()),
                                    ),
                                    boot: BootId::new(boot),
                                },
                                draining,
                            }),
                            actor: match state {
                                None => None,
                                Some(state) => Some(ClaimActorFacts {
                                    actor: actor.clone(),
                                    state: actor_state(&state)?,
                                    epoch: Epoch(row.get(3).map_err(store_failure)?),
                                    revision: StateRevision(row.get(4).map_err(store_failure)?),
                                    ready_at: row
                                        .get::<_, Option<i64>>(5)
                                        .map_err(store_failure)?
                                        .map(DurableInstant),
                                    next_due: row
                                        .get::<_, Option<i64>>(6)
                                        .map_err(store_failure)?
                                        .map(DurableInstant),
                                    formats: lash_durable::FormatSet::new(
                                        row.get::<_, String>(7).map_err(store_failure)?,
                                    ),
                                    requires: lash_durable::parse_requires(stored.as_deref())
                                        .map_err(|error| {
                                            corrupt("actor requirements", &error.to_string())
                                        })?,
                                    process_cancel: row.get(9).map_err(store_failure)?,
                                    immediate_cancel: row.get(10).map_err(store_failure)?,
                                    close_mail: row.get(11).map_err(store_failure)?,
                                    unfinished_close: row.get(12).map_err(store_failure)?,
                                }),
                            },
                        })
                    })();
                    Ok(result)
                },
            )
        })
        .await
    }
}
