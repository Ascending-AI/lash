//! The obligation relay and recovery leader lease laws (ADR 0109 §1) on
//! PostgreSQL.

use super::{pg_law_stores, reset, storage};

lash_conformance::obligation_relay_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres obligation relay conformance: database is not configured");
        return;
    };
    reset(storage.pool()).await;
    let (attachments, stores) = pg_law_stores(&storage);
    (
        (database_lock, attachments),
        lash_conformance::ObligationLawFixture {
            stores,
            prefix: "postgres".to_owned(),
        },
    )
});

lash_conformance::recovery_leader_tests!(|label| {
    let Some((database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres recovery leader conformance: database is not configured");
        return;
    };
    reset(storage.pool()).await;
    let (attachments, stores) = pg_law_stores(&storage);
    (
        (database_lock, attachments),
        lash_conformance::LeaseLawFixture {
            store: stores.recovery_leader(),
            name: format!("recovery:{label}"),
        },
    )
});

/// FIG-3879: the process registry stamps its rows with the database clock,
/// while a relay claims on its host clock. A process's terminal transaction
/// arms its terminal publication and its scope's parent-end plan due at once,
/// so a relay whose clock is behind the database takes both in its first
/// pass instead of waiting for its clock to catch up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_registry_armed_obligation_is_due_for_a_relay_clock_behind_the_database() {
    use lash_core_execution::store::{ObligationKey, ObligationKind};

    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping the Postgres registry clock law: database is not configured");
        return;
    };
    reset(storage.pool()).await;
    let (_attachments, stores) = pg_law_stores(&storage);
    let registry = stores.process_registry();
    let process = registry
        .register_process(lash_core_execution::ProcessRegistration::new(
            lash_core_execution::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core_execution::ProcessProvenance::host(),
            lash_core_execution::Lifetime::Detached,
        ))
        .await
        .expect("register the process")
        .id;
    registry
        .complete_process(
            &process,
            lash_core_execution::ProcessAwaitOutput::from_tool_output(
                lash_core_execution::ToolCallOutput::success(serde_json::json!({ "ended": true })),
            ),
            lash_core_execution::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete the process");
    let database_now: i64 = sqlx::query_scalar(
        "SELECT floor(extract(epoch FROM transaction_timestamp()) * 1000)::bigint",
    )
    .fetch_one(storage.pool())
    .await
    .expect("read the database clock");
    // A relay an hour behind the database.
    let relay_now = u64::try_from(database_now).expect("epoch") - 3_600_000;
    let page = std::num::NonZeroUsize::new(64).expect("page");
    let terminal = stores
        .obligation_ledger(ObligationKind::ProcessTerminal)
        .claim_due(relay_now, 60_000, page)
        .await
        .expect("claim the due terminal publications");
    assert!(
        terminal.iter().any(|claimed| matches!(
            &claimed.key,
            Ok(ObligationKey::ProcessTerminal { process_id }) if *process_id == process
        )),
        "the terminal publication is due for a relay behind the database: {terminal:?}"
    );
    let plans = stores
        .obligation_ledger(ObligationKind::ParentEnd)
        .claim_due(relay_now, 60_000, page)
        .await
        .expect("claim the due parent-end plans");
    assert!(
        plans.iter().any(|claimed| matches!(
            &claimed.key,
            Ok(ObligationKey::ParentEnd { parent_id, .. }) if parent_id.contains(process.as_str())
        )),
        "the process scope's parent-end plan is due for a relay behind the database: {plans:?}"
    );
}
