use std::sync::Arc;

use lash_core::RuntimePersistence;
use lash_core::testing::conformance::{
    SessionExecutionLeaseRenewalZeroRowHandles, SessionExecutionLeaseRenewalZeroRowInjector,
};
use lash_postgres_store::PostgresStorage;

mod support;

use support::{SharedDatabaseLock, database_url};

struct PostgresSessionExecutionLeaseRenewalZeroRowInjector {
    storage: Arc<PostgresStorage>,
}

#[async_trait::async_trait]
impl SessionExecutionLeaseRenewalZeroRowInjector
    for PostgresSessionExecutionLeaseRenewalZeroRowInjector
{
    async fn arm(&self, session_id: &str) {
        assert_eq!(session_id, "zero-row-session-lease-renewal");
        sqlx::raw_sql(
            "CREATE OR REPLACE FUNCTION lash_test_session_lease_renewal_zero_row()
             RETURNS trigger
             LANGUAGE plpgsql
             AS $$
             BEGIN
                 IF OLD.session_id = 'zero-row-session-lease-renewal' THEN
                     RETURN NULL;
                 END IF;
                 RETURN NEW;
             END;
             $$;
             CREATE TRIGGER lash_test_session_lease_renewal_zero_row
             BEFORE UPDATE OF lease_expires_at_ms ON lash_session_execution_leases
             FOR EACH ROW
             EXECUTE FUNCTION lash_test_session_lease_renewal_zero_row();",
        )
        .execute(self.storage.pool())
        .await
        .expect("arm Postgres zero-row renewal trigger");
    }

    async fn disarm(&self) {
        sqlx::raw_sql(
            "DROP TRIGGER lash_test_session_lease_renewal_zero_row
             ON lash_session_execution_leases;
             DROP FUNCTION lash_test_session_lease_renewal_zero_row();",
        )
        .execute(self.storage.pool())
        .await
        .expect("disarm Postgres zero-row renewal trigger");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_zero_row_session_execution_lease_renewal_is_refused_when_configured() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping Postgres zero-row renewal law: database URL is not set");
        return;
    };
    let _database_lock = SharedDatabaseLock::acquire(&database_url).await;
    let storage = Arc::new(
        PostgresStorage::connect(&database_url)
            .await
            .expect("connect Postgres zero-row renewal store"),
    );
    lash_core::testing::conformance::session_execution_lease_zero_row_renewal_is_refused(
        SessionExecutionLeaseRenewalZeroRowHandles {
            store: Arc::new(storage.unbound_session_store()) as Arc<dyn RuntimePersistence>,
            injector: Arc::new(PostgresSessionExecutionLeaseRenewalZeroRowInjector { storage }),
        },
    )
    .await;
}
