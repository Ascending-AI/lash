//! The drain for the canonical `SleepSpec` encoding (FIG-2968, FIG-2983).
//!
//! A journal written at effect generation 21 recorded a resolved
//! `Sleep { duration_ms }` command. This build journals the guest's intent as
//! `Sleep { spec }`, so the replay-hash fence over a generation-21 sleep row no
//! longer reconstructs: before the generation moved, a redrive of such a row
//! surfaced `ReplayMismatch` deep in the effect driver instead of a typed
//! refusal at open. Generation 22 refuses the whole journal at open instead.

use super::*;

const PRE_SLEEP_SPEC_EFFECT_GENERATION: i32 = 21;

fn sleep_effect_envelope() -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        lash_core::RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(
                durable_turn_scope("cutover-session", "cutover-turn"),
                "sleep-replay",
            )
            .expect("valid sleep effect address"),
            lash_core::RuntimeAttribution::for_turn("cutover-session", "cutover-turn", 1, 0),
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
/// the pre-cutover `{"type":"sleep","duration_ms":n}` shape the generation-21 writer
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

#[tokio::test]
async fn sqlite_refuses_pre_sleep_spec_effect_journal_at_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pre-sleep-spec-effects.db");
    let controller = SqliteRuntimeEffectController::open(
        &path,
        durable_turn_scope("cutover-session", "cutover-turn"),
    )
    .await
    .expect("create current effect store");
    let outcome = controller
        .execute_effect(
            sleep_effect_envelope(),
            RuntimeEffectLocalExecutor::unavailable(),
        )
        .await
        .expect("journal a sleep row at the current generation");
    assert!(matches!(outcome, RuntimeEffectOutcome::Sleep));
    drop(controller);

    let conn = rusqlite::Connection::open(&path).expect("open raw effect store");
    let envelope_json: String = conn
        .query_row(
            "SELECT envelope_json FROM runtime_effect_replay WHERE replay_key = ?1",
            rusqlite::params!["sleep-replay"],
            |row| row.get(0),
        )
        .expect("read journaled sleep envelope");
    let legacy_envelope = rewrite_sleep_command_to_resolved_duration(&envelope_json);
    let rewritten: serde_json::Value =
        serde_json::from_str(&legacy_envelope).expect("decode rewritten canonical envelope");
    let rewritten_command: serde_json::Value =
        serde_json::from_str(rewritten["json"].as_str().expect("serialized bytes"))
            .expect("decode rewritten envelope");
    assert!(rewritten_command["command"].get("duration_ms").is_some());
    assert!(rewritten_command["command"].get("spec").is_none());
    conn.execute(
        "UPDATE runtime_effect_replay SET envelope_json = ?1 WHERE replay_key = ?2",
        rusqlite::params![legacy_envelope, "sleep-replay"],
    )
    .expect("install the pre-cutover sleep row");
    conn.pragma_update(None, "user_version", PRE_SLEEP_SPEC_EFFECT_GENERATION)
        .expect("stamp the pre-SleepSpec effect generation");
    drop(conn);

    let error = match SqliteRuntimeEffectController::open(
        &path,
        durable_turn_scope("cutover-session", "cutover-turn"),
    )
    .await
    {
        Ok(_) => panic!("a pre-SleepSpec effect journal must be refused at open"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        "Error(\"Unsupported lash effect replay schema: this binary supports schema version 24, but the database reports version 21. There is no migration chain — drain affected sessions and recreate the whole Lash trust domain with this version. Reset the tombstones, await-event revocation ledger, effect journal, and Restate state together; see docs/adr/0049-session-ids-are-used-once.md.\")"
    );
}

#[tokio::test]
async fn sqlite_fresh_effect_journal_round_trips_a_sleep_across_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("fresh-sleep-spec-effects.db");
    let controller = SqliteRuntimeEffectController::open(
        &path,
        durable_turn_scope("cutover-session", "cutover-turn"),
    )
    .await
    .expect("create current effect store");
    let outcome = controller
        .execute_effect(
            sleep_effect_envelope(),
            RuntimeEffectLocalExecutor::unavailable(),
        )
        .await
        .expect("journal a sleep row at the current generation");
    assert!(matches!(outcome, RuntimeEffectOutcome::Sleep));
    drop(controller);

    let reopened = SqliteRuntimeEffectController::open(
        &path,
        durable_turn_scope("cutover-session", "cutover-turn"),
    )
    .await
    .expect("a journal this build wrote reopens at the current generation");
    reopened.start_replay();
    let replayed = reopened
        .execute_effect(sleep_effect_envelope(), failing_executor())
        .await
        .expect("the journaled sleep replays against its own fence");
    assert!(matches!(replayed, RuntimeEffectOutcome::Sleep));
}
