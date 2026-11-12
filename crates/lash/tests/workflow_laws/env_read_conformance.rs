//! Stored-environment conformance through the host API on both SQL stores.

use super::*;
use lash_execution_contracts::backend::StoreSet;
use lash_execution_contracts::process::ProjectionWatermark;

struct World {
    core: lash::LashCore,
    stores: Arc<dyn StoreSet>,
    _database: Option<lash_postgres_store::testing::IsolatedDatabase>,
    _attachments: Option<tempfile::TempDir>,
}

impl World {
    async fn new(postgres: bool) -> Self {
        let (stores, database, attachments): (Arc<dyn StoreSet>, _, _) = if postgres {
            let database = lash_postgres_store::testing::IsolatedDatabase::create(
                &lash_postgres_store::testing::required_database_url(),
            )
            .await;
            let storage = lash_postgres_store::testing::connect(database.url())
                .await
                .unwrap();
            let attachments = tempfile::tempdir().unwrap();
            let bytes = lash_sqlite_store::SqliteStoreSet::open(
                attachments.path().join("attachments.db"),
                lash_sqlite_store::SqliteSynchronous::Normal,
            )
            .await
            .unwrap()
            .attachment_store();
            (
                Arc::new(lash_postgres_store::PostgresStoreSet::new(&storage, bytes)),
                Some(database),
                Some(attachments),
            )
        } else {
            (
                Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap()),
                None,
                None,
            )
        };
        let core = builder_on_workers(
            stores.clone(),
            false,
            EchoComponent::Installed,
            lash::vm::WorkerService::default(),
        )
        .await
        .build(node_identity("env-read"))
        .unwrap();
        Self {
            core,
            stores,
            _database: database,
            _attachments: attachments,
        }
    }

    async fn publish(&self, pin: &lash::process::HostArtifactPin, account: &str) -> Publication {
        let base = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(1024))
            .component(ECHO_TOOLS, &())
            .component(WORKFLOW_SERVICES, &())
            .capability(
                lash::SlotId::new("account"),
                lash::CapabilityRef {
                    binding: lash::BindingId::new("binding:account"),
                    contract: lash::ContractRef::new("account", 1),
                    args: serde_json::json!({"account": account}),
                },
            )
            .build()
            .unwrap();
        let document = guarded(
            "function guarded(name: Text) -> Text\nkernel 1\nmetering statements\nbody { return name }\n",
        );
        let document = lash::workflow::document::DocumentBytes::from(document);
        publish(
            &self.core,
            pin,
            &document,
            &Name::new("guarded"),
            &base,
            None,
        )
        .await
        .unwrap()
    }

    async fn finish(&self, publication: &Publication, key: &str) -> lash::ProcessId {
        let process = start(
            &self.core,
            &publication.env_ref,
            &publication.definition,
            key,
            key,
        )
        .await;
        assert_eq!(finished(&self.core, &process).await, serde_json::json!(key));
        process
    }
}

/// ENV-READ-WHOLE: a finished run returns the published value, including its
/// body, and that value links exactly as the original publication does.
async fn finished_environment_relinks(postgres: bool) {
    let world = World::new(postgres).await;
    let pin = lash::process::HostArtifactPin::mint();
    let publication = world.publish(&pin, "first").await;
    let process = world.finish(&publication, "env-read-whole").await;
    let artifacts = world.core.host_artifacts();
    let (read, cost) = lash_core::operational_metrics::sql::collect(
        "host-artifacts.get_process_env",
        if postgres { "postgres" } else { "sqlite" },
        artifacts.get_process_env(&process),
    )
    .await;
    assert_eq!(cost.statements, 1, "one atomic environment read");
    eprintln!(
        "environment read SQL receipt: {}",
        serde_json::to_string(&cost).unwrap()
    );
    let read = read.unwrap().unwrap();
    assert_eq!(read, publication.environment);
    let (packages, _) = namespace(&world.core, &pin, &publication.environment).await;
    let target = lash::process::LinkTarget::Definition(publication.definition.id.clone());
    assert_eq!(
        artifacts
            .link_process_env(&read, &target, &packages, &Default::default())
            .await
            .unwrap(),
        artifacts
            .link_process_env(
                &publication.environment,
                &target,
                &packages,
                &Default::default(),
            )
            .await
            .unwrap()
    );
    world.core.shutdown().await.unwrap();
}

/// ENV-READ-HEAD: processes sharing one body read their own capability heads.
async fn shared_body_keeps_each_head(postgres: bool) {
    let world = World::new(postgres).await;
    let pin = lash::process::HostArtifactPin::mint();
    let first = world.publish(&pin, "first").await;
    let second = world.publish(&pin, "second").await;
    let split = |env: &lash::process::ProcessEnv| {
        lash_core_store::process_env::stored::split(&env.to_store_bytes().unwrap())
    };
    assert_eq!(
        split(&first.environment).body,
        split(&second.environment).body
    );
    assert_ne!(first.env_ref, second.env_ref);
    let first_process = world.finish(&first, "env-read-first").await;
    let second_process = world.finish(&second, "env-read-second").await;
    for (process, expected) in [
        (first_process, first.environment),
        (second_process, second.environment),
    ] {
        assert_eq!(
            world
                .core
                .host_artifacts()
                .get_process_env(&process)
                .await
                .unwrap(),
            Some(expected)
        );
    }
    world.core.shutdown().await.unwrap();
}

/// ENV-READ-LIFETIME: prune hides a retained environment; ending a record's
/// shares leaves its bytes readable until the last other head holder ends.
async fn absence_follows_prune_and_last_holder(postgres: bool) {
    let world = World::new(postgres).await;
    let pin = lash::process::HostArtifactPin::mint();
    let publication = world.publish(&pin, "retained").await;
    let pruned = world.finish(&publication, "env-read-pruned").await;
    let artifacts = world.core.host_artifacts();
    assert_eq!(
        artifacts
            .get_process_env(&lash::ProcessId::fixture("env-read-unknown"))
            .await
            .unwrap(),
        None
    );
    let cleanup_fence = lash::testing::quiesce_artifact_cleanup(&world.core).await;
    cleanup_fence.settle().await.unwrap();
    let registry = world.stores.process_registry();
    let record = registry.get_process(&pruned).await.unwrap().unwrap();
    let cutoff = record.updated_at_ms + 1;
    let result = registry
        .prune_terminal_processes(cutoff, None, ProjectionWatermark::NoProjector)
        .await
        .unwrap();
    assert_eq!(result.pruned_processes, 1);
    assert_eq!(artifacts.get_process_env(&pruned).await.unwrap(), None);
    let retained = world.finish(&publication, "env-read-retained").await;
    cleanup_fence.settle().await.unwrap();
    assert_eq!(
        artifacts.get_process_env(&retained).await.unwrap(),
        Some(publication.environment.clone())
    );
    // Apply the store shares explicitly, as the cleanup relay does. This
    // exercises missing bytes without changing the registry's retention.
    for process in [&pruned, &retained] {
        let cleanup = lash_core::ResolvedArtifactCleanup {
            referrer: lash_core::ArtifactReferrer::ProcessRecord(process.clone()),
            carries: Vec::new(),
        };
        world
            .stores
            .process_env_store()
            .end_process_env_referrer(&cleanup)
            .await
            .unwrap();
        world
            .stores
            .module_artifacts()
            .end_module_referrer(&cleanup)
            .await
            .unwrap();
    }
    assert_eq!(
        artifacts.get_process_env(&retained).await.unwrap(),
        Some(publication.environment.clone()),
        "the publisher pin still holds the head"
    );
    let cleanup = lash_core::ResolvedArtifactCleanup {
        referrer: lash_core::ArtifactReferrer::HostPin(pin),
        carries: Vec::new(),
    };
    world
        .stores
        .process_env_store()
        .end_process_env_referrer(&cleanup)
        .await
        .unwrap();
    world
        .stores
        .module_artifacts()
        .end_module_referrer(&cleanup)
        .await
        .unwrap();
    assert_eq!(artifacts.get_process_env(&retained).await.unwrap(), None);
    assert!(
        registry
            .get_process(&retained)
            .await
            .unwrap()
            .unwrap()
            .is_terminal()
    );
    drop(cleanup_fence);
    world.core.shutdown().await.unwrap();
}

macro_rules! stores {
    ($($law:ident),+ $(,)?) => {$(
        mod $law {
            #[tokio::test]
            async fn sqlite() { super::$law(false).await; }
            #[tokio::test]
            #[ignore = "requires PostgreSQL"]
            async fn postgres() { super::$law(true).await; }
        }
    )+};
}

stores!(
    finished_environment_relinks,
    shared_body_keeps_each_head,
    absence_follows_prune_and_last_holder,
);
