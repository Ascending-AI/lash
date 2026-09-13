use lash_sansio::SessionId;
use std::sync::Arc;

use lash_conformance::{
    SessionExecutionLeaseRenewalZeroRowHandles, SessionExecutionLeaseRenewalZeroRowInjector,
};
use lash_core::RuntimePersistence;
use lash_postgres_store::PostgresStorage;

use crate::support::{SharedDatabaseLock, database_url};

struct PostgresSessionExecutionLeaseRenewalZeroRowInjector {
    storage: Arc<PostgresStorage>,
}

#[async_trait::async_trait]
impl SessionExecutionLeaseRenewalZeroRowInjector
    for PostgresSessionExecutionLeaseRenewalZeroRowInjector
{
    async fn arm(&self, session_id: &SessionId) {
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

lash_conformance::session_execution_lease_renewal_tests!({
    let Some(database_url) = database_url() else {
        eprintln!("skipping Postgres zero-row renewal law: database URL is not set");
        return;
    };
    let database_lock = SharedDatabaseLock::acquire(&database_url).await;
    let storage = Arc::new(
        PostgresStorage::connect(&database_url)
            .await
            .expect("connect Postgres zero-row renewal store"),
    );
    (
        database_lock,
        SessionExecutionLeaseRenewalZeroRowHandles {
            store: Arc::new(storage.session_store("zero-row-session-lease-renewal"))
                as Arc<dyn RuntimePersistence>,
            injector: Arc::new(PostgresSessionExecutionLeaseRenewalZeroRowInjector { storage }),
        },
    )
});
