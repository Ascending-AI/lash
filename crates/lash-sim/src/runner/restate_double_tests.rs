//! The pending-tool scenario on the in-process Restate server double.

/// The pending-tool scenario runs on the concurrent server double and checks
/// the resulting state under multiple seeds.
#[tokio::test]
async fn pending_tool_completion_proof_runs_on_the_restate_server_double() {
    let seeds = crate::quick_seed_sweep(20) as u64;
    for seed in 0x5eed_7001_u64..0x5eed_7001 + seeds {
        let engine = crate::backend::SimEngine::new(seed)
            .await
            .expect("Restate test backend");
        let recorder = crate::invariants::HistoryRecorder::default();
        let proof =
            super::runtime_proofs::prove_pending_tool_completion_on(&engine, seed, &recorder)
                .await
                .unwrap_or_else(|error| panic!("seed {seed:#x}: pending tool proof: {error}"));
        let watch = engine.restate().server().drop_watch();
        let report = crate::invariants::check_engine(
            "restate-double/pending-tool-completion",
            seed,
            &recorder,
            &engine,
        )
        .await
        .expect("capture the history");
        report.print_quarantined();
        assert!(report.passed(), "{}", report.failure());
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
        drop(engine);
        assert!(
            watch.freed_within(std::time::Duration::from_secs(5)).await,
            "seed {seed:#x}: {} task(s) left",
            watch.live_tasks()
        );
    }
}

/// The sim's engine installs no wall-clock reconcile interval: a
/// deployment's own interval would relay due obligations wherever its store
/// reads happened to finish. A scenario
/// reconciles through `SessionDriver::reconcile` when it wants a pass; one
/// that never asks sees no reconcile ask.
#[tokio::test]
async fn the_server_double_runs_no_wall_clock_reconcile() {
    let engine = crate::backend::SimEngine::new(0x5eed_70f1)
        .await
        .expect("Restate test backend");
    super::runtime_proofs::prove_pending_tool_completion_on(
        &engine,
        0x5eed_70f1,
        &crate::invariants::HistoryRecorder::default(),
    )
    .await
    .expect("pending tool proof");
    let server = engine.restate().server();
    for invocation in server.invocations() {
        let Some(journal) = server.journal(&invocation.id) else {
            continue;
        };
        for entry in journal {
            assert!(
                !entry
                    .payload
                    .windows(b"reconcile:".len())
                    .any(|window| window == b"reconcile:"),
                "a wall-clock reconcile's ask reached the server double: {}",
                invocation.target
            );
        }
    }
}
