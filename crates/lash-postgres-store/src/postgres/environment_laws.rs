//! Environment owner laws on this backend's graph retirement and real cleanup relay.
use super::*;
use lash_durable::laws::environment::{self as env_laws, EnvironmentTier};
use lash_execution_contracts::backend::StoreSet;
#[async_trait::async_trait]
impl EnvironmentTier for Tier {
    fn packages(&self) -> Arc<dyn lash_durable::packages::PackageStore> {
        let stores: Arc<dyn StoreSet> = Arc::new(crate::PostgresStoreSet::new(
            &self._storage,
            Arc::new(lash_core_store::attachments::UnavailableAttachmentStore),
        ));
        stores.package_store()
    }
    async fn relay(&self) -> Result<(), String> {
        let stores: Arc<dyn StoreSet> = Arc::new(crate::PostgresStoreSet::new(
            &self._storage,
            Arc::new(lash_core_store::attachments::UnavailableAttachmentStore),
        ));
        let backend = lash_conformance::backend_over(stores);
        let relay = lash_core::runtime::artifact_cleanup::ArtifactCleanupRelay::over_backend(
            &backend,
            lash_core::ProcessEngineRegistry::default(),
        );
        let result = lash_core::runtime::obligations::relay::relay_due(
            &relay,
            backend.clock().as_ref(),
            std::num::NonZeroUsize::new(256).unwrap(),
        )
        .await
        .map_err(|e| e.to_string())?;
        if result.stalled != 0 {
            return Err(format!("cleanup stalled: {result:?}"));
        }
        Ok(())
    }
}
macro_rules! law {
    ($name:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            let tier = super::tier()
                .await
                .expect("environment laws require PostgreSQL");
            env_laws::$name(&tier).await.unwrap();
        }
    };
}
law!(env_lifetime);
law!(env_prepare);
law!(env_reincarnate);
law!(env_acquire_retire);
law!(env_crash_cuts_and_relay);

#[async_trait::async_trait]
impl env_laws::EnvironmentOwnerProbe for Tier {
    async fn retire_once(&self) -> Result<(), DurableError> {
        self.store
            .retire_step(lash_durable::domain::pages::RetirementBudget { nodes: 4096 })
            .await
            .map(drop)
    }
    async fn hold_owner(
        &self,
        id: &lash_core_store::environment::VersionOwnerId,
    ) -> Box<dyn LockHold> {
        let mut tx = self.pool.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar(
            "SELECT pg_backend_pid() FROM lash_environment_owners WHERE owner_id = $1 FOR UPDATE",
        )
        .bind(id.to_string())
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        Box::new(Held {
            tx,
            pool: self.pool.clone(),
            pid,
        })
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn env_publish_retire_acquire() {
    let tier = super::tier()
        .await
        .expect("environment lock law requires PostgreSQL");
    env_laws::env_publish_retire_acquire(&tier, &tier)
        .await
        .unwrap();
}

/// ENV-CORRUPT: undecodable owner facts refuse the peel as stored corruption;
/// they never become absent ownership or a retryable network-protocol failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn env_corrupt_owner_refuses_retirement() {
    use lash_durable::environment::{self as env, EnvironmentWrite};
    use lash_durable::{ActorKey, CommitLabel, DomainWrite, FormatSet, MailTx, NodeSpec};
    let tier = super::tier()
        .await
        .expect("corrupt owner law requires PostgreSQL");
    let actor = ActorKey::session("env-corrupt").unwrap();
    let mut create = MailTx::new();
    create.create_actor(actor.clone(), FormatSet::new("law-env-corrupt"));
    tier.store
        .commit_mail(create, CommitLabel::new("law.env.corrupt.create"))
        .await
        .unwrap();
    let node = tier
        .store
        .register_node(&NodeSpec {
            node: lash_durable::NodeId::new("env-corrupt-node"),
            decodes: vec![FormatSet::new("law-env-corrupt")],
            serves: vec![],
            capabilities: Default::default(),
            ttl_millis: 15_000,
        })
        .await
        .unwrap();
    let epoch = tier.store.claim(&node, 1).await.unwrap()[0].epoch;
    let body = env::assemble(None, []).unwrap();
    let owner = env::VersionOwner::preparing(body.id().clone());
    let head = env::EnvironmentHead {
        owner: owner.id().clone(),
        version: owner.version().clone(),
        input_frontier: Default::default(),
    };
    let mut tx = tier.store.begin(&actor, epoch).await.unwrap();
    tx.write(DomainWrite::Environment(
        EnvironmentWrite::prepare(owner.clone(), &body).unwrap(),
    ));
    tier.store
        .commit(tx, CommitLabel::new("law.env.corrupt.prepare"))
        .await
        .unwrap();
    let mut nodes = env::staging();
    env::stage_head(&mut nodes, &head).unwrap();
    let mut tx = tier.store.begin(&actor, epoch).await.unwrap();
    tx.stage(nodes.into_nodes().into_values());
    tx.write(DomainWrite::Environment(EnvironmentWrite::Publish {
        owner: owner.id().clone(),
    }));
    tx.write(DomainWrite::Environment(EnvironmentWrite::Head {
        expected: None,
        head: Some(head.clone()),
    }));
    tier.store
        .commit(tx, CommitLabel::new("law.env.corrupt.publish"))
        .await
        .unwrap();
    let mut tx = tier.store.begin(&actor, epoch).await.unwrap();
    tx.write(DomainWrite::Environment(EnvironmentWrite::Head {
        expected: Some(head.clone()),
        head: None,
    }));
    tier.store
        .commit(tx, CommitLabel::new("law.env.corrupt.release"))
        .await
        .unwrap();
    let anchor = env::version_node(&head.round()).unwrap();
    sqlx::query(
        "UPDATE lash_environment_owners SET version_id = 'undecodable' WHERE owner_id = $1",
    )
    .bind(owner.id().to_string())
    .execute(&tier.pool)
    .await
    .unwrap();
    let error = tier.retire().await.unwrap_err();
    assert!(
        matches!(
            error,
            DurableError::Store(lash_durable::StoreFailure {
                kind: lash_durable::StoreFailureKind::Corrupt,
                ..
            })
        ),
        "ownership corruption lost its typed cause: {error}"
    );
    assert!(
        tier.blob(&anchor.to_hex()).await.is_some(),
        "refused retirement peeled undecodable ownership"
    );
    assert!(
        matches!(
            tier.store
                .environments()
                .retire_abandoned_environment(owner.id())
                .await,
            Err(DurableError::Store(lash_durable::StoreFailure {
                kind: lash_durable::StoreFailureKind::Corrupt,
                ..
            }))
        ),
        "relay classified undecodable ownership as absent"
    );
}
