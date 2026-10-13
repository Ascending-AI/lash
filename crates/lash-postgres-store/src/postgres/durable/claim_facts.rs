//! A statement snapshot of scheduling and cancellation facts; no row locks.
use super::*;
use lash_durable::{ClaimActorFacts, ClaimFacts, ClaimNodeFacts};
use lash_store_sql::durable::ClaimFactsStatements;

static READ: LazyLock<String> = LazyLock::new(|| {
    ClaimFactsStatements::render(Dialect::postgres()).read.sql().replace(
        "FROM (SELECT",
        ", COALESCE($5::bigint, (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT) FROM (SELECT",
    )
});

impl PostgresDurableStore {
    pub(super) async fn read_claim_facts(
        &self,
        node: Option<&Owner>,
        actor: &ActorKey,
    ) -> Result<ClaimFacts, DurableError> {
        self.within(CommitCapacity::Work, async {
            let row = crate::observed_sql::query(&READ)
                .bind(node.map(|node| node.node.as_str()))
                .bind(actor.as_str())
                .bind(lash_durable::domain::CANCEL_MAIL)
                .bind(lash_durable::domain::SESSION_CLOSE_MAIL)
                .bind(self.injected_instant().transpose()?.map(|now| now.0))
                .fetch_one(crate::observed_sql::executor(&self.pools.work))
                .await
                .map_err(sqlx_failure)?;
            let boot: Option<String> = get(&row, 0)?;
            let node = boot
                .map(|boot| {
                    Ok::<_, DurableError>(ClaimNodeFacts {
                        owner: Owner {
                            node: NodeId::new(node.map_or("", |node| node.node.as_str())),
                            boot: BootId::new(boot),
                        },
                        draining: get(&row, 1)?,
                    })
                })
                .transpose()?;
            let state: Option<String> = get(&row, 2)?;
            let actor = match state {
                None => None,
                Some(state) => {
                    let stored: Option<String> = get(&row, 8)?;
                    Some(ClaimActorFacts {
                        actor: actor.clone(),
                        state: actor_state(&state)?,
                        epoch: Epoch(get(&row, 3)?),
                        revision: StateRevision(get(&row, 4)?),
                        ready_at: get::<Option<i64>>(&row, 5)?.map(DurableInstant),
                        next_due: get::<Option<i64>>(&row, 6)?.map(DurableInstant),
                        formats: FormatSet::new(get::<String>(&row, 7)?),
                        requires: lash_durable::parse_requires(stored.as_deref())
                            .map_err(|error| corrupt("actor requirements", &error.to_string()))?,
                        process_cancel: get(&row, 9)?,
                        immediate_cancel: get(&row, 10)?,
                        close_mail: get(&row, 11)?,
                        unfinished_close: get(&row, 12)?,
                    })
                }
            };
            Ok(ClaimFacts {
                observed_at: DurableInstant(get(&row, 13)?),
                node,
                actor,
            })
        })
        .await
    }
}
