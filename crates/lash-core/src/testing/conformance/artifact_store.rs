//! Conformance for [`crate::ProcessExecutionEnvStore`], the durable
//! artifact store that persists process-execution-env blobs for ADR-0013's
//! engine rebuild path.
//!
//! This is one of three logical keyspaces a durable backend multiplexes onto
//! one physical store; the other two (module artifacts and raw artifact bytes)
//! belong to the language crate's artifact-store trait, whose conformance lives
//! alongside that trait so the runtime kernel stays integration-agnostic. The
//! durable store crates run this suite plus that one plus a cross-namespace
//! isolation assertion so all three keyspaces are held to one contract.

use super::*;

/// A pair of [`crate::ProcessExecutionEnvStore`] handles opened against the same
/// durable backing store, used by the reopen-persistence case.
pub struct ReopenableProcessExecutionEnvStore {
    pub open: Arc<dyn crate::ProcessExecutionEnvStore>,
    pub reopen: Arc<dyn crate::ProcessExecutionEnvStore>,
}

fn sample_env_spec() -> crate::ProcessExecutionEnvSpec {
    crate::ProcessExecutionEnvSpec::new(crate::PluginOptions::default(), SessionPolicy::default())
}

/// Run the [`crate::ProcessExecutionEnvStore`] contract against the store
/// produced by `make`. `make` must return a fresh, empty store on each call.
pub async fn process_execution_env_store<F>(make: F)
where
    F: Fn() -> Arc<dyn crate::ProcessExecutionEnvStore>,
{
    let first = make();
    let second = make();
    assert_fresh_instances(&first, &second, "process_execution_env_store");
    drop((first, second));
    process_env_round_trips(make()).await;
    process_env_overwrite(make()).await;
}

/// Run the full contract plus a durable reopen check: bytes written through the
/// `open` handle must be visible through a `reopen` handle over the same store.
pub async fn process_execution_env_store_reopenable<F>(make: F)
where
    F: Fn() -> ReopenableProcessExecutionEnvStore,
{
    let probe = make();
    assert_fresh_instances(
        &probe.open,
        &probe.reopen,
        "process_execution_env_store_reopenable",
    );
    process_execution_env_store(|| make().open).await;
    process_env_survives_reopen(make()).await;
}

async fn process_env_round_trips(store: Arc<dyn crate::ProcessExecutionEnvStore>) {
    let spec = sample_env_spec();
    let env_ref = spec.stable_ref().expect("stable env ref");
    let bytes = spec.to_store_bytes().expect("encode env spec");
    assert!(
        store
            .get_process_execution_env(&env_ref)
            .await
            .expect("get missing env")
            .is_none(),
        "a fresh store must not resolve an unwritten process execution env"
    );

    store
        .put_process_execution_env(&env_ref, &bytes)
        .await
        .expect("put env");
    assert_eq!(
        store
            .get_process_execution_env(&env_ref)
            .await
            .expect("get env"),
        Some(bytes),
        "process execution env blob must round-trip"
    );
}

async fn process_env_overwrite(store: Arc<dyn crate::ProcessExecutionEnvStore>) {
    let env_ref = crate::ProcessExecutionEnvRef::new("process-env:overwrite");
    store
        .put_process_execution_env(&env_ref, b"first")
        .await
        .expect("put first env bytes");
    store
        .put_process_execution_env(&env_ref, b"second")
        .await
        .expect("overwrite env bytes");
    assert_eq!(
        store
            .get_process_execution_env(&env_ref)
            .await
            .expect("get overwritten env bytes"),
        Some(b"second".to_vec()),
        "re-putting a process execution env key must overwrite its bytes"
    );
}

async fn process_env_survives_reopen(reopenable: ReopenableProcessExecutionEnvStore) {
    let ReopenableProcessExecutionEnvStore { open, reopen } = reopenable;
    let env_ref = crate::ProcessExecutionEnvRef::new("process-env:reopen");
    open.put_process_execution_env(&env_ref, b"env-reopen")
        .await
        .expect("put env");
    assert_eq!(
        reopen
            .get_process_execution_env(&env_ref)
            .await
            .expect("get env after reopen"),
        Some(b"env-reopen".to_vec()),
        "process execution env blob must survive a store reopen"
    );
}
