//! The obligation relay and recovery leader lease laws (ADR 0109 §1) on
//! PostgreSQL.

use super::{pg_law_stores, reset, storage};

lash_conformance::obligation_relay_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!("skipping Postgres obligation relay conformance: database is not configured");
        return;
    };
    reset(storage.pool()).await;
    let (attachments, stores) = pg_law_stores(&storage);
    (
        (database_fixture, attachments),
        lash_conformance::ObligationLawFixture {
            stores,
            prefix: "postgres".to_owned(),
        },
    )
});

lash_conformance::recovery_leader_tests!(|label| {
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!("skipping Postgres recovery leader conformance: database is not configured");
        return;
    };
    reset(storage.pool()).await;
    let (attachments, stores) = pg_law_stores(&storage);
    (
        (database_fixture, attachments),
        lash_conformance::LeaseLawFixture {
            store: stores.recovery_leader(),
            name: format!("recovery:{label}"),
        },
    )
});

/// The kind check a build of this window declares on the cleanup table,
/// widened by `extra` labels.
fn cleanup_kind_check(extra: &[&str]) -> String {
    let labels = lash_core_execution::ArtifactReferrerKind::ALL
        .iter()
        .map(|kind| kind.as_str())
        .chain(extra.iter().copied())
        .map(|label| format!("'{label}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "ALTER TABLE lash_artifact_cleanup_obligations \
             DROP CONSTRAINT IF EXISTS ck_artifact_cleanup_obligations_kind; \
         ALTER TABLE lash_artifact_cleanup_obligations \
             ADD CONSTRAINT ck_artifact_cleanup_obligations_kind \
             CHECK (referrer_kind IN ({labels}));"
    )
}

/// ADR 0115 §5 on PostgreSQL: a later build's expand widens the named kind
/// check (an `ALTER`, no rebuild) and arms a cleanup row of its new kind.
/// This build refuses the row typed, stalls it and keeps its bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_referrer_kind_is_refused_typed_and_stalled() {
    let Some((_database_fixture, storage)) = storage().await else {
        eprintln!("skipping the Postgres unknown referrer kind law: database is not configured");
        return;
    };
    reset(storage.pool()).await;
    let label = lash_core_execution::SYNTHETIC_NEXT_REFERRER_KIND;
    let id = "obligation-written-by-a-later-build";
    sqlx::raw_sql(&cleanup_kind_check(&[label]))
        .execute(storage.pool())
        .await
        .expect("a later build widens the kind check");
    sqlx::query(
        "INSERT INTO lash_artifact_cleanup_obligations
             (referrer_kind, referrer_id, cleanup_json, obligation_id, obligation_state,
              obligation_due_at_ms)
         VALUES ($1, 'a-later-referrer', '{}', $2, 'due', 0)",
    )
    .bind(label)
    .bind(id)
    .execute(storage.pool())
    .await
    .expect("a later build arms a row of its kind");
    let (_attachments, stores) = pg_law_stores(&storage);
    lash_conformance::an_unknown_referrer_kind_is_refused_typed_and_stalled(
        stores,
        lash_core_execution::store::ObligationId::new(id),
        label,
    )
    .await;
    let kept: (String, String, String) = sqlx::query_as(
        "SELECT referrer_kind, referrer_id, cleanup_json
         FROM lash_artifact_cleanup_obligations WHERE obligation_id = $1",
    )
    .bind(id)
    .fetch_one(storage.pool())
    .await
    .expect("the row is kept");
    assert_eq!(
        kept,
        (
            label.to_owned(),
            "a-later-referrer".to_owned(),
            "{}".to_owned()
        )
    );
    sqlx::query("DELETE FROM lash_artifact_cleanup_obligations WHERE obligation_id = $1")
        .bind(id)
        .execute(storage.pool())
        .await
        .expect("remove the later build's row");
    sqlx::raw_sql(&cleanup_kind_check(&[]))
        .execute(storage.pool())
        .await
        .expect("restore this build's kind check");
}
