//! The pending-tool scenario on the in-process Restate server double.

use std::sync::{Arc, Mutex};

/// The pending-tool scenario on Restate, as a sim scenario runs it: the
/// in-process `lash-restate-test` server double under the scenario's seed
/// with serial scheduling, the turn inside an engine handler, the tool's
/// await key resolved through the engine's durable wait. Returns the proof,
/// the server's grant order and stall count, and a watch on its release.
async fn pending_tool_completion_on_the_restate_server_double(
    seed: u64,
) -> (
    crate::artifacts::PendingToolCompletionProof,
    Vec<(String, u32)>,
    u64,
    lash_restate_test::DropWatch,
) {
    let backend = lash_restate_test::backend(
        seed,
        lash_restate_test::ServerConfig::default()
            .scheduling(lash_restate_test::Scheduling::Serial),
    )
    .await
    .expect("Restate test backend");
    let driver_backend = backend.clone();
    let proof = super::runtime_proofs::prove_pending_tool_completion_on(
        Arc::new(backend.clone()),
        seed,
        Box::new(move |session, events| {
            tokio::spawn(async move {
                let turn_id = lash::TurnId::from("sim-pending-tool-turn");
                let admitted =
                    lash_core::AdmittedScope::unpinned(session.turn_scope(turn_id.clone()))
                        .map_err(|err| err.to_string())?;
                let report = Arc::new(Mutex::new(None));
                let slot = Arc::clone(&report);
                let attempt: lash_restate_test::HandlerAttempt = Arc::new(move |scoped| {
                    let session = session.clone();
                    let events = Arc::clone(&events);
                    let turn_id = turn_id.clone();
                    let slot = Arc::clone(&slot);
                    Box::pin(async move {
                        let result = session
                            .turn(lash::TurnInput::text(
                                super::runtime_proofs::PENDING_TOOL_PROMPT,
                            ))
                            .turn_id(turn_id)
                            .advanced()
                            .stream_to_with_scope(events.as_ref(), scoped)
                            .await
                            .map_err(|err| err.to_string());
                        *slot.lock().expect("report slot") = Some(result);
                    })
                });
                driver_backend.run_in_handler(admitted, attempt).await?;
                let recorded = report.lock().expect("report slot").take();
                recorded.ok_or_else(|| "the turn's handler recorded no report".to_owned())?
            })
        }),
    )
    .await
    .unwrap_or_else(|error| panic!("seed {seed:#x}: pending tool proof: {error}"));
    let trace = backend.server().schedule_trace();
    let preemptions = backend.server().stats().stall_preemptions;
    let watch = backend.server().drop_watch();
    (proof, trace, preemptions, watch)
}

/// Twenty seeds, each proven the same way, each run twice: under serial
/// scheduling one seed hands the turn between attempts in one order. The
/// runtime is current-thread, as a sim scenario's is: on it the tasks lash
/// spawns beside its handlers run between the holder's awaits, in one order.
#[tokio::test]
async fn pending_tool_completion_proof_runs_on_the_restate_server_double() {
    let started = std::time::Instant::now();
    for seed in 0x5eed_7001_u64..0x5eed_7001 + 20 {
        let (proof, trace, preemptions, watch) =
            pending_tool_completion_on_the_restate_server_double(seed).await;
        // A seed's run frees its server once the scenario is done with it:
        // a sweep builds one per seed (FIG-3721).
        assert!(
            watch.freed_within(std::time::Duration::from_secs(5)).await,
            "seed {seed:#x}: the scenario's server is freed; {} task(s) left",
            watch.live_tasks()
        );
        assert_eq!(proof.assistant_message, "done", "seed {seed:#x}");
        assert_eq!(proof.turn_index, 1, "seed {seed:#x}");
        assert!(proof.turn_suspended_before_completion, "seed {seed:#x}");
        assert!(matches!(
            proof.completion_outcome,
            lash_core::ResolveOutcome::Accepted
        ));
        assert!(matches!(
            proof.duplicate_completion_outcome,
            lash_core::ResolveOutcome::AlreadyResolved {
                terminal: lash_core::Resolution::Ok(_)
            }
        ));
        assert!(proof.turn_suspension_invariant.is_passed());
        assert!(proof.scheduler_resolution_invariant.is_passed());
        assert!(proof.final_result_invariant.is_passed(), "seed {seed:#x}");
        let (_, again, _, _) = pending_tool_completion_on_the_restate_server_double(seed).await;
        assert_eq!(trace, again, "seed {seed:#x}: one seed, one grant order");
        assert_eq!(preemptions, 0, "seed {seed:#x}: no turn moved on a stall");
    }
    println!(
        "pending-tool scenario on the Restate server double, serial: 20 seeds twice in {:?}",
        started.elapsed()
    );
}
