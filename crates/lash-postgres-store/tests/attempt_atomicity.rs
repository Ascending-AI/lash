//! The PostgreSQL counterpart of the tool-attempt atomicity gate.
//!
//! On Restate the hazard is real: the journal is **ordinal-addressed**, so a
//! nested command emitted from inside a recorded `ToolAttempt` shifts every
//! later ordinal and redrive fails with `RT0016`.
//!
//! The PostgreSQL effect-replay tier is **key-addressed**: every effect claims
//! its own `(scope_id, replay_key)` row under a fenced lease
//! (`postgres/effect_replay.rs`, `runtime/effect/effect_replay_driver.rs`).
//! A nested effect claims its own key, so there is no ordinal to shift. That is
//! a claim, and this module proves it: a recorded attempt whose body emits a
//! nested effect is crashed and redriven on a second, independently-connected
//! host, and both the attempt and its nested effect replay their recorded
//! terminals byte-for-byte without re-executing either body.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::{
    EffectHost, ExecutionScope, RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectKind,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeInvocation, RuntimeScope,
};
use lash_postgres_store::{PostgresEffectHost, PostgresStorage};

mod support;

use support::{SharedDatabaseLock, database_url};

const SESSION: &str = "pg-attempt-atomicity-session";
const TURN: &str = "pg-attempt-atomicity-turn";
const ATTEMPT_KEY: &str = "pg-attempt-atomicity:attempt";
const NESTED_KEY: &str = "pg-attempt-atomicity:attempt:nested";

fn attempt_invocation() -> RuntimeInvocation {
    RuntimeInvocation::effect(
        RuntimeScope::for_turn(SESSION, TURN, 0, 0),
        "pg-attempt-atomicity-attempt",
        RuntimeEffectKind::ToolAttempt,
        ATTEMPT_KEY,
    )
}

/// The nested effect the attempt body emits. It carries its own replay key,
/// derived from the attempt's key exactly as `process_effect_invocation` derives
/// a nested process command's key in production.
fn nested_invocation() -> RuntimeInvocation {
    RuntimeInvocation::effect(
        RuntimeScope::for_turn(SESSION, TURN, 0, 0),
        "pg-attempt-atomicity-nested",
        RuntimeEffectKind::ToolAttempt,
        NESTED_KEY,
    )
}

/// A recorded `ToolAttempt` — the unit whose body must not be re-entered on
/// redrive. Both the outer attempt and the nested command it emits are journaled
/// as attempts here so each one's body execution is observable.
fn attempt_envelope(invocation: RuntimeInvocation, call_id: &str) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        invocation,
        RuntimeEffectCommand::ToolAttempt {
            call: lash_core::PreparedToolCall {
                call_id: call_id.to_string(),
                tool_id: lash_core::ToolId::from("tool:pg_attempt_atomicity".to_string()),
                tool_name: "pg_attempt_atomicity".to_string(),
                args: serde_json::Value::Null,
                replay: None,
                prepared_payload: serde_json::Value::Null,
            },
            execution_grant: None,
            attempt: 1,
            max_attempts: 1,
        },
    )
}

fn attempt_outcome(call_id: &str, value: &str) -> RuntimeEffectOutcome {
    RuntimeEffectOutcome::ToolAttempt {
        launch: Box::new(lash_core::ToolAttemptLaunch::Done {
            record: Box::new(lash_core::ToolCallRecord {
                call_id: Some(call_id.to_string()),
                tool: "pg_attempt_atomicity".to_string(),
                args: serde_json::Value::Null,
                output: lash_core::ToolCallOutput::success(serde_json::json!(value)),
                duration_ms: 0,
            }),
        }),
        triggers: Vec::new(),
    }
}

fn projected_output(outcome: &RuntimeEffectOutcome) -> String {
    let RuntimeEffectOutcome::ToolAttempt { launch, .. } = outcome else {
        panic!("expected a tool-attempt outcome");
    };
    let lash_core::ToolAttemptLaunch::Done { record } = launch.as_ref() else {
        panic!("expected a completed tool attempt");
    };
    record.output.value_for_projection().to_string()
}

async fn reset(storage: &PostgresStorage) {
    for statement in
        ["DELETE FROM lash_runtime_effect_replay WHERE scope_id LIKE '%pg-attempt-atomicity%'"]
    {
        sqlx::query(statement)
            .execute(storage.pool())
            .await
            .expect("reset the PostgreSQL attempt-atomicity effect rows");
    }
}

/// Runs the hazard shape on one host: a recorded attempt whose body emits a
/// nested journal command through the *same* controller.
///
/// Returns how many times each body actually executed.
async fn run_attempt_with_nested_command(host: &PostgresEffectHost) -> (usize, usize, String) {
    let scoped = host
        .scoped(ExecutionScope::turn(SESSION, TURN))
        .expect("scoped PostgreSQL effect controller");
    let attempt_body_runs = Arc::new(AtomicUsize::new(0));
    let nested_body_runs = Arc::new(AtomicUsize::new(0));
    let outcome = {
        let attempt_body_runs = Arc::clone(&attempt_body_runs);
        let nested_body_runs = Arc::clone(&nested_body_runs);
        let controller = scoped.controller();
        controller
            .execute_effect(
                attempt_envelope(attempt_invocation(), "pg-attempt-atomicity-outer"),
                RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
                    attempt_body_runs.fetch_add(1, Ordering::SeqCst);
                    // The nested emission: a second journal command issued from
                    // inside the recorded body, through the same controller.
                    let nested_body_runs = Arc::clone(&nested_body_runs);
                    let nested = controller
                        .execute_effect(
                            attempt_envelope(nested_invocation(), "pg-attempt-atomicity-nested"),
                            RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
                                nested_body_runs.fetch_add(1, Ordering::SeqCst);
                                Ok(attempt_outcome("pg-attempt-atomicity-nested", "nested"))
                            }),
                        )
                        .await;
                    assert!(
                        nested.is_ok(),
                        "the nested command must execute on the key-addressed tier: {nested:?}"
                    );
                    Ok(attempt_outcome("pg-attempt-atomicity-outer", "outer"))
                }),
            )
            .await
            .expect("recorded attempt completes on the PostgreSQL tier")
    };
    (
        attempt_body_runs.load(Ordering::SeqCst),
        nested_body_runs.load(Ordering::SeqCst),
        projected_output(&outcome),
    )
}

/// The key-addressed tier law: crash after a recorded attempt emitted a nested
/// command, redrive on a fresh host, and both effects replay their recorded
/// terminals without re-entering either body. No ordinal exists, so nothing can
/// shift.
#[tokio::test(flavor = "multi_thread")]
async fn attempt_with_nested_command_redrives_identically_on_the_key_addressed_tier() {
    let Some(database_url) = database_url() else {
        eprintln!(
            "skipping the PostgreSQL attempt-atomicity law: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let _database_lock = SharedDatabaseLock::acquire(&database_url).await;

    let first_storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect the first PostgreSQL attempt-atomicity host");
    reset(&first_storage).await;
    // First execution runs in normal (non-strict) mode: nothing is recorded yet.
    let first_host = first_storage.effect_host();
    let (attempt_runs, nested_runs, first_outcome) =
        run_attempt_with_nested_command(&first_host).await;
    assert_eq!(
        attempt_runs, 1,
        "the recorded attempt body runs once on first execution"
    );
    assert_eq!(
        nested_runs, 1,
        "the nested command body runs once on first execution"
    );
    assert_eq!(
        first_outcome, "\"outer\"",
        "first execution records the attempt terminal"
    );

    // The crash: drop the first host and its pool entirely, then redrive the
    // identical work on a second, independently-connected host — a different
    // process as far as the effect journal is concerned.
    drop(first_host);
    drop(first_storage);

    let second_storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect the redriving PostgreSQL attempt-atomicity host");
    // Strict replay: the redriving host refuses to execute anything it does not
    // find recorded, so a re-executed body would fail loudly rather than pass
    // silently.
    let second_host = second_storage.effect_host();
    second_host.start_replay();
    let (redriven_attempt_runs, redriven_nested_runs, redriven_outcome) =
        run_attempt_with_nested_command(&second_host).await;
    assert_eq!(
        redriven_attempt_runs, 0,
        "redrive replays the recorded attempt terminal without re-entering the body"
    );
    assert_eq!(
        redriven_nested_runs, 0,
        "the nested command replays from its own key without re-executing; a \
         key-addressed journal has no ordinal for it to shift"
    );
    assert_eq!(
        redriven_outcome, "\"outer\"",
        "redrive yields the identical recorded terminal"
    );

    // Each effect owns its own row, keyed independently: that is *why* nesting
    // is safe here rather than an accident of ordering.
    let keys: Vec<String> = sqlx::query_scalar(
        "SELECT replay_key FROM lash_runtime_effect_replay
         WHERE scope_id LIKE '%pg-attempt-atomicity%' ORDER BY replay_key",
    )
    .fetch_all(second_storage.pool())
    .await
    .expect("read the PostgreSQL attempt-atomicity effect rows");
    assert_eq!(
        keys,
        vec![ATTEMPT_KEY.to_string(), NESTED_KEY.to_string()],
        "the attempt and its nested command each claimed their own replay key"
    );

    reset(&second_storage).await;
}
