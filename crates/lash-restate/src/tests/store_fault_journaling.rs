//! A fault of the store inside a recorded process step ends the attempt: it
//! is never the step's journaled answer, which every replay would serve
//! (FIG-4649).
//!
//! Each law drives one process command through `execute_effect` over a SQLite
//! registry behind the fault decorator. The contract: the faulted attempt
//! ends at the step with nothing journaled for it, and the next attempt runs
//! the step again and answers. A control law pins the other half: a typed
//! refusal is the step's journaled answer on the first attempt.

use super::recording_context::runtime_invocation;
use super::*;

/// The faults a busy or failing SQLite or PostgreSQL substrate hands the
/// registry, as the stores' own mappers type them.
fn store_faults() -> Vec<(&'static str, lash_core::StoreError)> {
    vec![
        ("write contention", lash_core::StoreError::Contended),
        (
            "sqlite storage failure",
            lash_core::StoreError::StorageFailure {
                backend: "sqlite",
                message: "disk I/O error".to_string(),
            },
        ),
        (
            "postgres storage failure",
            lash_core::StoreError::StorageFailure {
                backend: "postgres",
                message: "pool timed out while waiting for an open connection".to_string(),
            },
        ),
    ]
}

fn tick_type() -> lash_core::ProcessEventType {
    lash_core::ProcessEventType {
        name: "producer.tick".to_string(),
        payload_schema: lash_core::JsonSchema::any(),
        semantics: lash_core::ProcessEventSemanticsSpec::default(),
    }
}

fn emit_tick(process_id: &ProcessId, replay_key: &str) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        runtime_invocation(RuntimeEffectKind::Process, replay_key),
        RuntimeEffectCommand::process(ProcessCommand::EmitEvent {
            process_id: process_id.clone(),
            request: lash_core::ProcessEventAppendRequest::new(
                "producer.tick",
                serde_json::json!({}),
            ),
        }),
    )
}

#[tokio::test]
pub(super) async fn an_emit_whose_append_meets_a_store_fault_is_never_journaled_as_its_answer() {
    for (name, fault) in store_faults() {
        let context = Arc::new(RecordingContext::default());
        let host = RestateRuntimeEffectController::new_for_test(context.clone());
        let stores = memory_process_stores().await;
        let registry: Arc<dyn ProcessRegistry> = stores.registry.clone();
        let process_id = registry
            .register_process(external_registration().with_extra_event_types([tick_type()]))
            .await
            .expect("register the emitting process")
            .id;

        stores
            .registry
            .fail_next_event_append(PluginError::from(fault));
        let ended = context
            .attempt
            .run(host.execute_effect(
                emit_tick(&process_id, "emit-under-fault"),
                registry_local_executor(registry.clone()),
            ))
            .await
            .expect_err(&format!(
                "{name}: the store's fault ends the attempt instead of answering the step"
            ));
        assert!(
            ended.effect.contains("process-emit-event"),
            "{name}: the attempt ended at the append step: {ended:?}"
        );

        let outcome = host
            .execute_effect(
                emit_tick(&process_id, "emit-under-fault"),
                registry_local_executor(registry.clone()),
            )
            .await
            .unwrap_or_else(|error| panic!("{name}: the next attempt appends: {error:?}"));
        assert!(
            matches!(
                outcome,
                RuntimeEffectOutcome::Process {
                    result: ProcessEffectOutcome::EmitEvent { .. }
                }
            ),
            "{name}: {outcome:?}"
        );
    }
}

#[tokio::test]
pub(super) async fn an_await_whose_read_meets_a_store_fault_is_never_journaled_as_its_answer() {
    for (name, fault) in store_faults() {
        let context = Arc::new(RecordingContext::default());
        let host = RestateRuntimeEffectController::new_for_test(context.clone());
        let stores = memory_process_stores().await;
        let registry: Arc<dyn ProcessRegistry> = stores.registry.clone();
        let process_id = registry
            .register_process(external_registration())
            .await
            .expect("register the awaited process")
            .id;
        let output = process_success(serde_json::json!({ "done": true }));
        registry
            .complete_process(
                &process_id,
                output.clone(),
                lash_core::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete the awaited process");
        context.resolve_process_terminal(&process_id, &output);
        let await_it = || {
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "await-under-fault"),
                RuntimeEffectCommand::process(ProcessCommand::Await {
                    process_id: process_id.clone(),
                }),
            )
        };

        stores
            .registry
            .set_process_read_error(Some(PluginError::from(fault)));
        let ended = context
            .attempt
            .run(host.execute_effect(await_it(), registry_local_executor(registry.clone())))
            .await
            .expect_err(&format!(
                "{name}: the store's fault ends the attempt instead of answering the step"
            ));
        assert!(
            ended.effect.contains("process-await-observation"),
            "{name}: the attempt ended at the observation step: {ended:?}"
        );

        stores.registry.set_process_read_error(None);
        let outcome = host
            .execute_effect(await_it(), registry_local_executor(registry.clone()))
            .await
            .unwrap_or_else(|error| panic!("{name}: the next attempt observes: {error:?}"));
        assert!(
            matches!(
                outcome,
                RuntimeEffectOutcome::Process {
                    result: ProcessEffectOutcome::Await { .. }
                }
            ),
            "{name}: {outcome:?}"
        );
    }
}

#[tokio::test]
pub(super) async fn a_refused_append_is_the_steps_journaled_answer() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new_for_test(context.clone());
    let stores = memory_process_stores().await;
    let registry: Arc<dyn ProcessRegistry> = stores.registry.clone();
    let process_id = registry
        .register_process(external_registration().with_extra_event_types([tick_type()]))
        .await
        .expect("register the emitting process")
        .id;

    stores.registry.fail_next_event_append(PluginError::from(
        lash_core::StoreError::StoredDataCorrupt {
            record_kind: "ProcessEvent",
            message: "negative sequence".to_string(),
        },
    ));
    // No attempt is open: a retried fault here would fail the test, so the
    // refusal returned is the one the step journaled.
    let refused = host
        .execute_effect(
            emit_tick(&process_id, "emit-refused"),
            registry_local_executor(registry.clone()),
        )
        .await
        .expect_err("corrupt stored data is the step's answer");
    assert_eq!(
        refused.code,
        lash_core::RuntimeErrorCode::RuntimeStoreCorrupt
    );
    assert!(refused.is_terminal());
}
