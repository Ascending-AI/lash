//! The obligation relay and recovery leader lease laws (ADR 0109 §1) on
//! SQLite.

use std::sync::Arc;

use lash_core_execution::StoreSet as _;

use super::SUBSTRATE;
use crate::backend_fixture::TestBackend;

lash_conformance::obligation_relay_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let stores: Arc<dyn lash_core_execution::StoreSet> = Arc::new((*backend).clone());
    (
        backend,
        lash_conformance::ObligationLawFixture {
            stores,
            prefix: "sqlite".to_owned(),
        },
    )
});

lash_conformance::recovery_leader_tests!(|label| {
    let backend = TestBackend::open(SUBSTRATE).await;
    let store = backend.recovery_leader();
    (
        backend,
        lash_conformance::LeaseLawFixture {
            store,
            name: format!("recovery:{label}"),
        },
    )
});

/// ADR 0115 §5 on SQLite: a later build writes a cleanup row whose referrer
/// kind this build does not know straight into the durable core. The kind
/// CHECK admits any non-empty label, so that needs no table rebuild; this
/// build refuses the row typed, stalls it and keeps its bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_referrer_kind_is_refused_typed_and_stalled() {
    let backend = TestBackend::open(SUBSTRATE).await;
    let label = lash_core_execution::SYNTHETIC_NEXT_REFERRER_KIND;
    let id = "core:obligation-written-by-a-later-build";
    let raw = backend.raw(lash_sqlite_store::SqliteDatabase::DurableCore);
    raw.execute(
        "INSERT INTO artifact_cleanup_obligations \
             (referrer_kind, referrer_id, cleanup_json, obligation_id, obligation_state, \
              obligation_due_at_ms) \
         VALUES (?1, 'a-later-referrer', '{}', ?2, 'due', 0)",
        rusqlite::params![label, id],
    )
    .expect("a later build writes its kind without a table rebuild");
    let stores: Arc<dyn lash_core_execution::StoreSet> = Arc::new((*backend).clone());
    lash_conformance::an_unknown_referrer_kind_is_refused_typed_and_stalled(
        stores,
        lash_core_execution::store::ObligationId::new(id),
        label,
    )
    .await;
    let kept: (String, String, String) = raw
        .query_row(
            "SELECT referrer_kind, referrer_id, cleanup_json \
             FROM artifact_cleanup_obligations WHERE obligation_id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("the row is kept");
    assert_eq!(
        kept,
        (
            label.to_owned(),
            "a-later-referrer".to_owned(),
            "{}".to_owned()
        )
    );
}
