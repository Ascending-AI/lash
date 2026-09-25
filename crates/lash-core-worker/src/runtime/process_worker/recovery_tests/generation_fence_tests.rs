//! L6 on the native worker (FIG-3571): an incarnation's start record names
//! the executable generation its engine ran it as, and a claim under a build
//! whose engine runs another generation — or of a record written before the
//! stamp existed — parks the process `RetiredGeneration` before its engine is
//! entered. A fresh process is stamped with the generation it runs as.

use super::*;

const CURRENT: &str = "blake3:current-generation";
const RETIRED: &str = "blake3:retired-generation";

/// Runs as [`CURRENT`] and counts its runs: a run is the first thing an
/// incarnation this build admits does.
struct StampedEngine {
    runs: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::ProcessEngine for StampedEngine {
    fn kind(&self) -> &'static str {
        "stamped"
    }

    fn program_identity(
        &self,
        _payload: &serde_json::Value,
    ) -> Option<crate::ExecutableGeneration> {
        Some(crate::ExecutableGeneration::new(CURRENT))
    }

    async fn run(
        &self,
        context: crate::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!({"process_id": context.registration().id}),
            ))
            .into(),
        )
    }
}

async fn await_park(registry: &Arc<dyn ProcessRegistry>, process_id: &ProcessId) -> ProcessRecord {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let record = registry
                .get_process(process_id)
                .await
                .expect("read process")
                .expect("the process is retained");
            if record.park.is_some() {
                return record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the retired incarnation parks")
}

/// A stamp-less or foreign start record parks the process after one claim,
/// naming the generation its start recorded, with its engine never entered;
/// the drain counts the park under that generation.
#[tokio::test]
async fn an_incarnation_started_under_another_generation_parks_before_its_engine_runs() {
    for (case, found) in [
        ("foreign", Some(crate::ExecutableGeneration::new(RETIRED))),
        ("stampless", None),
    ] {
        let runs = Arc::new(AtomicUsize::new(0));
        let (_worker, registry, run_handle, env_ref) = worker_with_engine(
            1,
            Arc::new(StampedEngine {
                runs: Arc::clone(&runs),
            }),
            Arc::new(LateBoundProcessWork::default()),
        )
        .await;
        let process_id = ProcessId::from(format!("generation-fence-{case}"));
        registry
            .register_process(engine_registration(
                process_id.clone(),
                "stamped",
                env_ref,
                serde_json::Value::Null,
            ))
            .await
            .expect("register the stamped process");
        registry
            .record_first_started(
                &process_id,
                ProcessStarted {
                    owner: LeaseOwnerIdentity::opaque("previous-build", "previous-incarnation"),
                    fencing_token: 0,
                    attempt: 1,
                    started_at_ms: 1,
                    generation: found.clone(),
                },
            )
            .await
            .expect("a previous build started the incarnation");

        let _ = run_handle
            .enable_and_drive()
            .await
            .expect("drive the retired incarnation");
        let parked = await_park(&registry, &process_id).await;

        assert_eq!(
            runs.load(Ordering::SeqCst),
            0,
            "{case}: the engine never ran"
        );
        assert!(!parked.is_terminal(), "{case}: a park is non-terminal");
        assert_eq!(
            parked.outcome, None,
            "{case}: a park writes no terminal evidence"
        );
        let park = parked.park.as_deref().expect("parked");
        assert_eq!(
            park.reason,
            crate::store::ParkReason::retired_process_generation(
                crate::ExecutableGenerationRefusal {
                    found: found.clone(),
                    current: Some(crate::ExecutableGeneration::new(CURRENT)),
                }
            ),
            "{case}: the park is the typed retired generation, naming what the start recorded"
        );
        assert_eq!(park.attempts, 1, "{case}: one claim, one refusal");
        assert_eq!(
            parked
                .first_started
                .as_deref()
                .and_then(|started| started.generation.clone()),
            found,
            "{case}: the stamp is never re-derived on recovery"
        );
        let summary = registry
            .summarize_parked_processes()
            .await
            .expect("summarize parked processes");
        assert_eq!(
            summary
                .by_reason
                .get(&crate::store::ParkReasonCode::RetiredGeneration),
            Some(&1),
            "{case}"
        );
        assert_eq!(
            summary.retired_by_executable_generation,
            found
                .iter()
                .map(|generation| (generation.clone(), 1))
                .collect(),
            "{case}: the drain counts the park under the generation its start recorded"
        );
    }
}

/// A fresh process is stamped with the generation its engine runs it as at
/// its first claim, and runs.
#[tokio::test]
async fn a_fresh_process_is_stamped_with_the_generation_it_runs_as() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (_worker, registry, run_handle, env_ref) = worker_with_engine(
        1,
        Arc::new(StampedEngine {
            runs: Arc::clone(&runs),
        }),
        Arc::new(LateBoundProcessWork::default()),
    )
    .await;
    let process_id = ProcessId::from("generation-fence-fresh");
    registry
        .register_process(engine_registration(
            process_id.clone(),
            "stamped",
            env_ref,
            serde_json::Value::Null,
        ))
        .await
        .expect("register the stamped process");
    let _ = run_handle
        .enable_and_drive()
        .await
        .expect("drive the fresh process");
    await_terminal(&registry, &process_id).await;
    let record = registry
        .get_process(&process_id)
        .await
        .expect("read process")
        .expect("the process is retained");
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(
        record
            .first_started
            .as_deref()
            .and_then(|started| started.generation.clone()),
        Some(crate::ExecutableGeneration::new(CURRENT)),
        "the first claim stamps the generation the engine runs the process as"
    );
    assert!(record.park.is_none());
}
