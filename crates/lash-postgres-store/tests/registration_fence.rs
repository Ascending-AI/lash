//! PostgreSQL registration lifts the scope fence of every host bound to the
//! registry (ADR 0049). PostgreSQL is storage only (ADR 0104): the fence lives
//! in the bound engine host, so reinstating the bound hosts after the
//! registration commits is the registry's one fence lift.

use std::sync::Arc;

use lash_core_execution::{
    AdmittedScope, EffectAddress, EffectHost, RuntimeAttribution, RuntimeEffectCommand,
    RuntimeEffectEnvelope, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
};
use lash_postgres_store::PostgresStorage;
use lash_sansio::ProcessId;

use crate::support::{SharedDatabaseLock, database_url, reset};

fn envelope(admitted: &AdmittedScope, effect_id: &str) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        lash_core_execution::RuntimeEffectInvocation::new(
            EffectAddress::new(admitted.scope().clone(), effect_id)
                .expect("the fence effect carries an admitted scope"),
            RuntimeAttribution::none(),
            effect_id,
        ),
        RuntimeEffectCommand::LanguageRuntimeValue {
            operation: effect_id.to_string(),
        },
    )
}

async fn admission(
    host: &dyn EffectHost,
    admitted: &AdmittedScope,
    effect_id: &str,
) -> Result<(), lash_core_execution::RuntimeErrorCode> {
    host.scoped(admitted.clone())
        .expect("scope binds")
        .controller()
        .execute_effect(
            envelope(admitted, effect_id),
            RuntimeEffectLocalExecutor::testing(|_| async {
                Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
                    value: serde_json::json!({ "ran": true }),
                })
            }),
        )
        .await
        .map(|_| ())
        .map_err(|err| err.code)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_reinstates_every_bound_host() {
    let Some(url) = database_url() else {
        eprintln!("skipping Postgres registration fence test: database URL is not set");
        return;
    };
    let _lock = SharedDatabaseLock::acquire(&url).await;
    let storage = PostgresStorage::connect(&url)
        .await
        .expect("connect PostgreSQL");
    reset(storage.pool()).await;
    let registry = Arc::new(storage.process_registry_with_wake_delivery_config(
        lash_core_execution::WakeDeliveryConfig::default(),
    )) as Arc<dyn lash_core_execution::ProcessRegistry>;
    // A host whose fence lives outside this registry's store, bound by hand.
    let journal = tempfile::tempdir().expect("journal directory");
    let other: Arc<dyn EffectHost> = Arc::new(
        lash_sqlite_store::SqliteEffectHost::open(&journal.path().join("effects.db"))
            .await
            .expect("open the other host"),
    );
    registry.bind_effect_host(&other);
    let process_id = ProcessId::from("reused-across-hosts");
    other
        .retire_effect_journal(lash_core_execution::EffectJournalRetirement::process(
            process_id.as_str(),
        ))
        .await
        .expect("the other host fences the id");
    let stale = AdmittedScope::process(lash_core_execution::ProcessRef::new(
        process_id.clone(),
        lash_core_execution::ProcessIncarnation::from_registration_sequence(1),
    ));
    assert_eq!(
        admission(other.as_ref(), &stale, "while-fenced").await,
        Err(lash_core_execution::RuntimeErrorCode::EffectScopeRetired)
    );
    registry
        .register_process(
            lash_core_execution::ProcessRegistration::new(
                process_id.clone(),
                lash_core_execution::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core_execution::RecoveryContract::ExternallyOwned,
                lash_core_execution::ProcessProvenance::host(),
                lash_core_execution::ProcessLifecyclePolicy::new(
                    lash_core_execution::ParentScope::Host,
                    lash_core_execution::OnParentEnd::Abandon,
                ),
            )
            .with_admitted_identity(
                lash_core_execution::AdmittedProcessIdentity::for_testing(
                    lash_core_execution::ProcessIdentity::new("test"),
                ),
            ),
        )
        .await
        .expect("register the id");
    let record = registry
        .get_process(&process_id)
        .await
        .expect("read the process record")
        .expect("the process is registered");
    admission(
        other.as_ref(),
        &AdmittedScope::process(lash_core_execution::ProcessRef::from_record(&record)),
        "after-registration",
    )
    .await
    .expect("the registration reinstated the bound host's scope");
}
