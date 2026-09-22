//! The Postgres half of the drain for the canonical `SleepSpec` encoding
//! (FIG-2968, FIG-2983).
//!
//! A journal written at component 95 recorded a resolved
//! `Sleep { duration_ms }` command. This build journals the guest's intent as
//! `Sleep { spec }`, so the replay-hash fence over a component-95 sleep row no
//! longer reconstructs: before the component moved, a redrive of such a row
//! surfaced `ReplayMismatch` deep in the effect driver instead of a typed
//! refusal at open. Component 96 refuses the whole store at open instead.

use lash_core::{
    ExecutionScope, RuntimeEffectCommand, RuntimeEffectController, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome,
};
use lash_postgres_store::PostgresStorage;

use crate::support::{SharedDatabaseLock, database_url};

const PRE_SLEEP_SPEC_COMPONENT_VERSION: i32 = 95;
const REPLAY_KEY: &str = "sleep-replay";

async fn storage() -> Option<(SharedDatabaseLock, PostgresStorage)> {
    let url = database_url()?;
    let database_lock = SharedDatabaseLock::acquire(&url).await;
    let storage = PostgresStorage::connect(&url)
        .await
        .expect("connect postgres");
    Some((database_lock, storage))
}

fn sleep_effect_envelope() -> lash_core::RuntimeEffectEnvelope {
    lash_core::RuntimeEffectEnvelope::new(
        lash_core::RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(
                ExecutionScope::turn("sleep-cutover-session", "sleep-cutover-turn"),
                REPLAY_KEY,
            )
            .expect("valid sleep effect address"),
            lash_core::RuntimeAttribution::for_turn(
                "sleep-cutover-session",
                "sleep-cutover-turn",
                1,
                0,
            ),
            "sleep",
        ),
        RuntimeEffectCommand::Sleep {
            spec: lash_core::SleepSpec::For { duration_ms: 5 },
        },
    )
}

/// Rewrite the stored canonical envelope -- the exact serialized bytes plus the
/// BLAKE3 verdict over those same bytes -- from the current
/// `{"type":"sleep","spec":{"kind":"for","duration_ms":n}}` command back to
/// the pre-cutover `{"type":"sleep","duration_ms":n}` shape the component-95 writer
/// produced, re-hashing under the unchanged `lash-runtime-effect-envelope/v3`
/// domain so the row is what that writer would have committed rather than a
/// hand-broken fence.
fn rewrite_sleep_command_to_resolved_duration(canonical_json: &str) -> String {
    let mut canonical: serde_json::Value =
        serde_json::from_str(canonical_json).expect("decode canonical sleep envelope");
    let encoded = canonical["json"]
        .as_str()
        .expect("canonical envelope carries its serialized bytes");
    let mut envelope: serde_json::Value =
        serde_json::from_str(encoded).expect("decode journaled sleep envelope");
    let command = envelope
        .pointer_mut("/command")
        .and_then(serde_json::Value::as_object_mut)
        .expect("journaled sleep command");
    let spec = command
        .remove("spec")
        .expect("current fixture carries spec");
    let duration_ms = spec
        .get("duration_ms")
        .cloned()
        .expect("relative sleep spec carries duration_ms");
    command.insert("duration_ms".to_string(), duration_ms);
    let legacy = serde_json::to_string(&envelope).expect("encode pre-cutover sleep envelope");
    canonical["hash"] =
        serde_json::Value::String(lash_sansio::core_support::blake3_domain_hash_hex(
            "lash-runtime-effect-envelope/v3",
            legacy.as_bytes(),
        ));
    canonical["json"] = serde_json::Value::String(legacy);
    serde_json::to_string(&canonical).expect("encode pre-cutover canonical envelope")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_refuses_pre_sleep_spec_effect_journal_at_open() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres pre-SleepSpec open gate: database is not configured");
        return;
    };
    let pool = storage.pool().clone();
    let controller = storage.runtime_effect_controller(ExecutionScope::turn(
        "sleep-cutover-session",
        "sleep-cutover-turn",
    ));
    sqlx::query("DELETE FROM lash_runtime_effect_replay WHERE replay_key = $1")
        .bind(REPLAY_KEY)
        .execute(&pool)
        .await
        .expect("clear any prior sleep fixture row");
    let outcome = controller
        .execute_effect(
            sleep_effect_envelope(),
            RuntimeEffectLocalExecutor::unavailable(),
        )
        .await
        .expect("journal a sleep row at the current component");
    assert!(matches!(outcome, RuntimeEffectOutcome::Sleep));

    let envelope_json: String = sqlx::query_scalar(
        "SELECT envelope_json FROM lash_runtime_effect_replay WHERE replay_key = $1",
    )
    .bind(REPLAY_KEY)
    .fetch_one(&pool)
    .await
    .expect("read journaled sleep envelope");
    let legacy_envelope = rewrite_sleep_command_to_resolved_duration(&envelope_json);
    let rewritten: serde_json::Value =
        serde_json::from_str(&legacy_envelope).expect("decode rewritten canonical envelope");
    let rewritten_command: serde_json::Value =
        serde_json::from_str(rewritten["json"].as_str().expect("serialized bytes"))
            .expect("decode rewritten envelope");
    assert!(rewritten_command["command"].get("duration_ms").is_some());
    assert!(rewritten_command["command"].get("spec").is_none());
    sqlx::query("UPDATE lash_runtime_effect_replay SET envelope_json = $1 WHERE replay_key = $2")
        .bind(legacy_envelope)
        .bind(REPLAY_KEY)
        .execute(&pool)
        .await
        .expect("install the pre-cutover sleep row");
    sqlx::query(
        "UPDATE lash_schema_versions SET version = $1 WHERE component = 'lash-postgres-store'",
    )
    .bind(PRE_SLEEP_SPEC_COMPONENT_VERSION)
    .execute(&pool)
    .await
    .expect("stamp the pre-SleepSpec component schema");

    let result = PostgresStorage::from_pool(pool.clone()).await;

    sqlx::query(
        "UPDATE lash_schema_versions SET version = $1 WHERE component = 'lash-postgres-store'",
    )
    .bind(PostgresStorage::schema_version())
    .execute(&pool)
    .await
    .expect("restore current component schema");
    sqlx::query("DELETE FROM lash_runtime_effect_replay WHERE replay_key = $1")
        .bind(REPLAY_KEY)
        .execute(&pool)
        .await
        .expect("remove pre-cutover fixture");

    let message = match result {
        Ok(_) => panic!("a pre-SleepSpec effect journal must be refused at open"),
        Err(error) => error.to_string(),
    };
    let expected = PostgresStorage::schema_version();
    let source = expected - 1;
    assert_eq!(
        message,
        format!(
            "store backend error: Postgres schema component `lash-postgres-store` has version {PRE_SLEEP_SPEC_COMPONENT_VERSION}, expected {expected}. That database was provisioned by an older build: component {PRE_SLEEP_SPEC_COMPONENT_VERSION} predates this build's component {expected} and has no applicable migration. The component schema is normally a reject-and-recreate boundary. This build declares a forward migration into component {expected} only from component {source}, so component {PRE_SLEEP_SPEC_COMPONENT_VERSION} has no upgrade path. Drain the affected sessions and recreate the whole Lash trust domain with this build: provision the database from the DDL artifact this build ships (`PostgresStorage::schema_ddl()`, committed as crates/lash-postgres-store/schema.sql), then reset the session tombstones, the await-event revocation ledger, the effect journal, and the Restate state together — any one of them left behind still refers to sessions the recreated database does not have. docs/adr/0081-destructive-schema-changes-are-currently-reject-and-recreate.md records why this boundary refuses instead of migrating. This gate is unconditional; SchemaCheck::WarnOnly does not relax it. This database was last written by lash release {}.",
            env!("CARGO_PKG_VERSION")
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_fresh_effect_journal_round_trips_a_sleep_across_reopen() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres fresh sleep journal round-trip: database is not configured");
        return;
    };
    let pool = storage.pool().clone();
    let scope = ExecutionScope::turn("sleep-roundtrip-session", "sleep-roundtrip-turn");
    let replay_key = "sleep-roundtrip-replay";
    let envelope = lash_core::RuntimeEffectEnvelope::new(
        lash_core::RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(scope.clone(), replay_key)
                .expect("valid sleep effect address"),
            lash_core::RuntimeAttribution::for_turn(
                "sleep-roundtrip-session",
                "sleep-roundtrip-turn",
                1,
                0,
            ),
            "sleep",
        ),
        RuntimeEffectCommand::Sleep {
            spec: lash_core::SleepSpec::For { duration_ms: 5 },
        },
    );
    sqlx::query("DELETE FROM lash_runtime_effect_replay WHERE replay_key = $1")
        .bind(replay_key)
        .execute(&pool)
        .await
        .expect("clear any prior round-trip row");

    let controller = storage.runtime_effect_controller(scope.clone());
    let outcome = controller
        .execute_effect(envelope.clone(), RuntimeEffectLocalExecutor::unavailable())
        .await
        .expect("journal a sleep row at the current component");
    assert!(matches!(outcome, RuntimeEffectOutcome::Sleep));

    let reopened = PostgresStorage::from_pool(pool.clone())
        .await
        .expect("a store this build provisioned reopens at the current component");
    let replay_controller = reopened.runtime_effect_controller(scope);
    replay_controller.start_replay();
    let replayed = replay_controller
        .execute_effect(envelope, RuntimeEffectLocalExecutor::unavailable())
        .await
        .expect("the journaled sleep replays against its own fence");
    assert!(matches!(replayed, RuntimeEffectOutcome::Sleep));

    sqlx::query("DELETE FROM lash_runtime_effect_replay WHERE replay_key = $1")
        .bind(replay_key)
        .execute(&pool)
        .await
        .expect("remove round-trip fixture");
}
