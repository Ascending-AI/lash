//! The store tiers the host laws run on, each under the Restate server
//! double. The host decorates the tier's store set before the double runs
//! over it, so every store read the runtime makes goes through the decorator.

use std::sync::Arc;

pub enum Tier {
    SqliteMemory,
    SqliteFile,
    Postgres,
}

pub struct Double {
    pub double: lash_restate_test::RestateTestBackend<dyn lash::StoreSet>,
    _keep: Vec<Box<dyn std::any::Any + Send + Sync>>,
}

/// The tier's double over `decorate(stores)`, or `None` for the PostgreSQL
/// tier when no database is configured (and none is required).
#[expect(
    clippy::expect_used,
    reason = "test fixture: a broken store setup aborts the test"
)]
pub async fn double(
    tier: Tier,
    seed: u64,
    decorate: impl FnOnce(Arc<dyn lash::StoreSet>) -> Arc<dyn lash::StoreSet>,
) -> Option<Double> {
    let config = lash_restate_test::ServerConfig::default();
    let hooks = lash_restate_test::DeploymentHooks::default;
    match tier {
        Tier::SqliteMemory => {
            let double =
                lash_restate_test::backend_with_store_set(seed, config, hooks(), |clock| async {
                    Ok(decorate(Arc::new(
                        lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
                            .await
                            .expect("SQLite memory stores"),
                    )))
                })
                .await
                .expect("SQLite memory Restate double");
            Some(Double {
                double,
                _keep: Vec::new(),
            })
        }
        Tier::SqliteFile => {
            let root = tempfile::tempdir().expect("SQLite store directory");
            let path = root.path().to_path_buf();
            let double =
                lash_restate_test::backend_with_store_set(seed, config, hooks(), |clock| async {
                    Ok(decorate(Arc::new(
                        lash_sqlite_store::SqliteStoreSet::open_with_clock(&path, clock)
                            .await
                            .expect("SQLite file stores"),
                    )))
                })
                .await
                .expect("SQLite file Restate double");
            Some(Double {
                double,
                _keep: vec![Box::new(root)],
            })
        }
        Tier::Postgres => {
            let url = std::env::var("LASH_POSTGRES_DATABASE_URL")
                .ok()
                .filter(|url| !url.trim().is_empty());
            assert!(
                url.is_some() || std::env::var("LASH_REQUIRE_POSTGRES").as_deref() != Ok("1"),
                "LASH_POSTGRES_DATABASE_URL must be set when LASH_REQUIRE_POSTGRES=1"
            );
            let url = url?;
            let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
            let storage = lash_postgres_store::PostgresStorage::connect(database.url())
                .await
                .expect("open provisioned PostgreSQL storage");
            let attachments = tempfile::tempdir().expect("attachment directory");
            let attachment_path = attachments.path().to_path_buf();
            let double =
                lash_restate_test::backend_with_store_set(seed, config, hooks(), |clock| async {
                    Ok(decorate(Arc::new(
                        lash_postgres_store::PostgresStoreSet::with_clock(
                            &storage,
                            Arc::new(lash::persistence::FileAttachmentStore::new(
                                &attachment_path,
                            )),
                            lash_core::WakeDeliveryConfig::default(),
                            clock,
                        ),
                    )))
                })
                .await
                .expect("PostgreSQL Restate double");
            Some(Double {
                double,
                _keep: vec![Box::new(database), Box::new(storage), Box::new(attachments)],
            })
        }
    }
}
