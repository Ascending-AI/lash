//! A reattached trigger emission reports the deliveries its committed attempt
//! started (FIG-4272).
//!
//! The load driver's cron tick emits one occurrence inside a workflow handler
//! and reads back the output of every process the emission started. When the
//! deployment dies after the target committed and before the handler
//! answered, Restate replays the handler from its journal and the driver
//! reattaches to that replay. The replayed emission finds the occurrence and
//! its bound delivery already in the store. Its report must still name the
//! process the committed attempt started, so the tick answers that process and
//! its committed output rather than an empty result, and nothing starts twice.

use super::*;

/// What one handler attempt answered: the processes its emission reported
/// started, and each one's recorded outcome.
type Answers = Arc<Mutex<Vec<(Vec<ProcessId>, Vec<serde_json::Value>)>>>;

const SOURCE_TYPE: &str = "ui.button.pressed";

/// The committed attempt emits the tick, lets its target commit an output and
/// dies before it answers. The server replays the handler into a redrive that
/// emits the same tick and answers. The redrive must answer the process and
/// output the committed attempt produced.
pub(super) async fn a_reattached_emission_reports_the_deliveries_its_committed_attempt_started<
    Stores: lash_core::StoreSet + ?Sized,
>(
    backend: &lash_restate_test::RestateTestBackend<Stores>,
    seed: u64,
) {
    let lash = backend.lash_backend();
    let registry = lash.process_registry();
    let triggers = lash.trigger_store();
    let process_env_store = lash.process_env_store();
    lash_core::testing::process_execution_env_fixture(process_env_store.as_ref()).await;
    let source_key = lash_core::facade_support::empty_trigger_source_key(SOURCE_TYPE)
        .expect("the tick's source key");
    super::restate_redrive::register_fig811_subscription(
        triggers.as_ref(),
        &format!("fig4272-register-{seed:x}"),
        "fig4272-tick-target",
        &source_key,
    )
    .await;
    let router = Arc::new(
        lash_core::facade_support::TriggerRouter::new(
            Arc::clone(&triggers),
            registry_process_wiring(Arc::clone(&registry)),
        )
        .with_process_artifacts(
            process_env_store,
            lash_core::testing::process_engine_fixture(),
        ),
    );
    let tick = format!("fig4272-tick-{seed:x}");
    let occurrence = {
        let tick = tick.clone();
        move || {
            lash_core::TriggerOccurrenceRequest::new(
                SOURCE_TYPE,
                source_key.clone(),
                serde_json::json!({ "tick": tick }),
                tick.clone(),
            )
        }
    };
    let answers: Answers = Arc::default();
    let attempt = |crash: bool| -> lash_restate_test::HandlerAttempt {
        let router = Arc::clone(&router);
        let registry = Arc::clone(&registry);
        let answers = Arc::clone(&answers);
        let occurrence = occurrence.clone();
        let tick = tick.clone();
        Arc::new(move |scoped| {
            let router = Arc::clone(&router);
            let registry = Arc::clone(&registry);
            let answers = Arc::clone(&answers);
            let occurrence = occurrence.clone();
            let tick = tick.clone();
            Box::pin(async move {
                let report = router
                    .emit(occurrence(), &scoped)
                    .await
                    .expect("the tick emits");
                let started = report.started_process_ids();
                if crash {
                    // The target runs on its own workflow and commits its
                    // marker: the effect the committed attempt owns.
                    for process_id in &started {
                        registry
                            .complete_process(
                                process_id,
                                process_success(serde_json::json!({ "marked": tick })),
                                lash_core::ProcessCompletionAuthority::workflow_key(format!(
                                    "fig4272-target-{process_id}"
                                )),
                            )
                            .await
                            .expect("the target commits its marker");
                    }
                }
                let mut outputs = Vec::new();
                for process_id in &started {
                    let record = registry
                        .get_process(process_id)
                        .await
                        .expect("read the started process")
                        .expect("the started process is retained");
                    outputs.push(
                        serde_json::to_value(record.outcome()).expect("encode the process outcome"),
                    );
                }
                answers.lock().unwrap().push((started, outputs));
                assert!(
                    !crash,
                    "the committed attempt dies after its target committed, before it answers"
                );
            })
        })
    };
    backend
        .run_crashed_then_redriven(
            lash_core::AdmittedScope::runtime_operation(format!("fig4272-cron-tick-{seed:x}")),
            attempt(true),
            attempt(false),
        )
        .await
        .expect("the reattached tick completes without a journal mismatch");

    let answers = answers.lock().unwrap().clone();
    assert_eq!(answers.len(), 2, "each attempt answered once: {answers:?}");
    let (committed_started, committed_outputs) = &answers[0];
    assert_eq!(
        committed_started.len(),
        1,
        "the committed attempt started the tick's one delivery"
    );
    assert!(
        !committed_outputs[0].is_null(),
        "the committed attempt's target committed its marker: {committed_outputs:?}"
    );
    assert_eq!(
        answers[1], answers[0],
        "the reattached tick answers the process and output its committed attempt produced"
    );
    let retained = registry
        .list_processes(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..Default::default()
        })
        .await
        .expect("list processes")
        .into_iter()
        .map(|record| record.id)
        .collect::<Vec<_>>();
    assert_eq!(
        &retained, committed_started,
        "the reattach starts no second process"
    );
}

pub(super) const SEEDS: std::ops::Range<u64> = 0x4272_0000..0x4272_0004;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reattached_emission_reports_the_deliveries_its_committed_attempt_started_on_sqlite() {
    for seed in SEEDS {
        let backend = lash_restate_test::backend(seed, lash_restate_test::ServerConfig::default())
            .await
            .expect("build the Restate test backend");
        a_reattached_emission_reports_the_deliveries_its_committed_attempt_started(&backend, seed)
            .await;
    }
}
