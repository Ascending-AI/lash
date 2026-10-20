use super::*;

/// Diagnostic latency cannot pace a completed lane: hold only the
/// observer's first binding read while two real requests finish.
#[tokio::test]
async fn diagnostic_latency_cannot_pace_a_completed_lane() {
    use lash_core::testing::{Script, StoreOp};
    let reads = Script::new();
    let held = reads.on(StoreOp::run_of_input).before().pause();
    let stores = lash::sqlite::SqliteStoreSet::memory().await.unwrap();
    let backend = durable_backend(stores).unwrap();
    let spec = crate::latency::default_cases(2, 1).remove(0);
    let timing = Arc::new(ProviderTiming::default());
    let core = build_core(backend, &spec, Arc::clone(&timing), None, &None, None).unwrap();
    let session_id = SessionId::fixture("diagnostic-latency-law");
    let durable = core
        .session(session_id.clone())
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            latency_llm_profile_spec().unwrap().wire_model,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1),
            lash_core::NoProgressBudget::bounded(12),
        )))
        .await
        .unwrap();
    let session = LaneSession::Durable(durable);
    let poll_store: Arc<dyn lash::persistence::RuntimeStore> =
        reads.wrap("diagnostic-only", core.backend().session_store_factory());
    let diagnostics = Diagnostics::default();
    let ctx = SampleCtx {
        diagnostics: &diagnostics,
        spec: &spec,
        session: &session,
        poll_store: &poll_store,
        session_id: &session_id,
        timing: &timing,
    };
    let mut measured = Box::pin(async {
        let first = measure_send(&ctx, 0, 0, None, LoopClocks::default(), || {})
            .await
            .unwrap();
        let second = measure_send(&ctx, 0, 1, None, LoopClocks::default(), || {})
            .await
            .unwrap();
        vec![first, second]
    });
    held.reached_by(&mut measured, 1).await;
    let completed = tokio::time::timeout(Duration::from_secs(5), &mut measured).await;
    let paced = completed.is_err();
    held.open_one();
    let samples = match completed {
        Ok(samples) => samples,
        Err(_) => measured.await,
    };
    let (samples, receipt) = diagnostics.finish(samples).await.unwrap();
    assert_eq!(receipt.joined, 2);
    assert_eq!(receipt.pending_at_teardown, 0);
    assert_eq!(samples.len(), 2);
    assert_ne!(samples[0].input_id, samples[1].input_id);
    reads.finish().unwrap();
    core.shutdown().await.unwrap();
    assert!(
        !paced,
        "marker discovery paced the completed lane's next request"
    );
}

/// Bounded diagnostic ownership refuses overflow and joins every timeout
/// by identity, even when completion order differs from sample order.
#[tokio::test(start_paused = true)]
async fn diagnostic_saturation_and_timeout_keep_every_disposition() {
    let diagnostics = Diagnostics::default();
    let session = SessionId::fixture("saturation");
    let mut completed = Vec::new();
    for index in 0..=diagnostics::PENDING_LIMIT {
        let input = lash_core::InputId::fixture(format!("diagnostic-{index}"));
        let pending = diagnostics.start(&session, &input, std::future::pending());
        let mut sample = complete_sample(index);
        sample.session_id = session.to_string();
        sample.input_id = input.to_string();
        completed.push(CompletedSample { sample, pending });
    }
    completed.reverse();
    let (samples, receipt) = diagnostics.finish(completed).await.unwrap();
    assert_eq!(samples.len(), diagnostics::PENDING_LIMIT + 1);
    assert_eq!(receipt.pending_peak, diagnostics::PENDING_LIMIT);
    assert_eq!(receipt.refused, 1);
    assert_eq!(receipt.timed_out, diagnostics::PENDING_LIMIT);
    assert_eq!(receipt.pending_at_teardown, 0);
    assert_eq!(samples[0].diagnostic, DiagnosticDisposition::Refused);
    assert!(samples.iter().all(|sample| sample.error.is_some()));

    struct Dropped(Arc<tokio::sync::Notify>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }
    let dropped = Arc::new(tokio::sync::Notify::new());
    let owner = Dropped(Arc::clone(&dropped));
    let diagnostics = Diagnostics::default();
    diagnostics.start(
        &session,
        &lash_core::InputId::fixture("cancelled"),
        async move {
            let _owner = owner;
            std::future::pending().await
        },
    );
    drop(diagnostics);
    tokio::time::timeout(Duration::from_secs(1), dropped.notified())
        .await
        .unwrap();
}

fn complete_sample(index: usize) -> Sample {
    let stamp = Stamp {
        monotonic_ns: 1,
        unix_ns: 1,
    };
    Sample {
        case: "fast",
        lane: index % 16,
        index,
        cold: false,
        status: "answered".to_owned(),
        session_id: String::new(),
        input_id: String::new(),
        clocks: SampleClocks {
            request: stamp,
            accepted: Some(stamp),
            acceptance_acknowledged: stamp,
            admission: Some(stamp),
            terminal: Some(stamp),
            terminal_commit_returned: Some(stamp),
            completion: stamp,
            first_activity: Some(stamp),
            first_visible: Some(stamp),
            first_delta: Some(stamp),
            admission_observed: Some(stamp),
            terminal_observed: Some(stamp),
            host_first_activity: Some(stamp),
            host_first_visible: Some(stamp),
            host_first_delta: Some(stamp),
        },
        admission_to_first_visible_ms: Some(1.0),
        admission_to_first_activity_ms: Some(1.0),
        admission_observation_lag_ms: Some(1.0),
        terminal_observation_lag_ms: Some(1.0),
        request_to_accept_ms: 1.0,
        accept_to_admission_ms: Some(1.0),
        accept_to_applied_ms: Some(1.0),
        admission_to_first_delta_ms: Some(1.0),
        admission_to_settled_ms: Some(1.0),
        settled_to_complete_ms: Some(1.0),
        send_to_completion_ms: 6.0,
        provider_ms: Some(5.0),
        overhead_ms: Some(1.0),
        model_span_ms: Some(5.0),
        poll_detect_ms: Some(1.0),
        poller_timed_out: false,
        diagnostic: DiagnosticDisposition::Complete,
        sqlite_workers_at_request: Vec::new(),
        sqlite_workers_at_completion: Vec::new(),
        process_cpu_ms_at_end: None,
        process_rss_kb_at_end: None,
        error: None,
        loop_clocks: LoopClocks::default(),
        loop_spans: LoopSpans::default(),
    }
}

/// FAST-POPULATION: a certificate covers every answered row and clock
/// in the fixed 16-lane population, never a filtered percentile subset.
#[test]
fn fast_population_refuses_incomplete_clocks_failures_and_wrong_geometry() {
    let mut fixtures = Vec::new();
    for fault in [
        "complete",
        "overhead-n-1",
        "missing-clocks",
        "failure",
        "errors",
        "lanes",
        "actual-lanes",
    ] {
        let mut samples: Vec<_> = (0..10_000).map(complete_sample).collect();
        let mut errors = Vec::new();
        let mut spec = crate::latency::default_cases(10_000, 16).remove(0);
        match fault {
            "overhead-n-1" => samples
                .iter_mut()
                .skip(1)
                .for_each(|s| s.overhead_ms = None),
            "missing-clocks" => samples[9999].clocks.admission = None,
            "failure" => samples[9999].status = "failed".to_owned(),
            "errors" => errors.push("lane failed".to_owned()),
            "lanes" => spec.lanes = 8,
            _ => {}
        }
        let mut case = CaseReport::assemble(&spec, &samples, errors, Duration::ZERO);
        case.actual_lanes = if fault == "actual-lanes" {
            8
        } else {
            spec.lanes
        };
        let report = build_report(serde_json::json!({}), vec![case]);
        fixtures.push(
            serde_json::json!({"fixture": fault, "rows": samples.len(), "verdict": report.verdict}),
        );
    }
    println!(
        "fast-population-refusals.json={}",
        serde_json::to_string(&fixtures).unwrap()
    );
    for fixture in fixtures {
        assert_eq!(
            fixture["verdict"]["pass"],
            fixture["fixture"] == "complete",
            "{fixture}"
        );
    }
}

#[tokio::test]
async fn startup_store_phases_are_complete_and_ordered() {
    use lash_core_ids::perf_witness::startup::Phase;
    use lash_core_ids::perf_witness::startup::Recorder;
    let recorder = Recorder::install().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let _stores = lash_sqlite_store::SqliteStoreSet::open(
        dir.path().join("startup.db"),
        lash_sqlite_store::SqliteSynchronous::Full,
    )
    .await
    .unwrap();
    let markers = recorder.snapshot();
    let positions: Vec<_> = [
        Phase::StoreOpenStarted,
        Phase::StoreOpened,
        Phase::StoreSetupFinished,
    ]
    .iter()
    .map(|phase| {
        markers
            .iter()
            .position(|marker| &marker.phase == phase)
            .unwrap_or_else(|| panic!("missing startup phase {phase:?}"))
    })
    .collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(
        markers
            .windows(2)
            .all(|pair| pair[0].since_entry_ns <= pair[1].since_entry_ns)
    );
}

/// LATENCY-BINDING: the grace case discovers an unbound input at the
/// short floor on successive reads, before it can hold a terminal wait.
#[tokio::test(start_paused = true)]
async fn grace_binding_discovery_stays_at_floor_after_unbound_reads() {
    use lash_core::testing::runtime_helpers::LayeredBackend;
    use lash_core::testing::{Script, StoreOp};

    let reads = Script::new();
    let first = reads.on(StoreOp::follow_facts).after().pause();
    let second = reads.on(StoreOp::follow_facts).nth(2).after().pause();
    let third = reads.on(StoreOp::follow_facts).nth(3).before().pause();
    let backend = durable_backend(lash::sqlite::SqliteStoreSet::memory().await.unwrap()).unwrap();
    let backend = LayeredBackend::over(backend)
        .map_session_store_factory(|inner| reads.wrap("grace-observer", inner))
        .into_backend();
    let spec = crate::latency::default_cases(1, 1)
        .into_iter()
        .find(|case| case.name == "grace")
        .unwrap();
    let pacing = spec.follower.pacing().unwrap().follow;
    assert_eq!(
        pacing.initial(),
        lash::ObserverPacing::standard().follow.initial()
    );
    let core = build_core(
        backend,
        &spec,
        Arc::new(ProviderTiming::default()),
        None,
        &None,
        None,
    )
    .unwrap();
    let session = core
        .session(SessionId::fixture("grace-binding-floor"))
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            latency_llm_profile_spec().unwrap().wire_model,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1),
            lash_core::NoProgressBudget::bounded(12),
        )))
        .await
        .unwrap();
    let handle = session
        .send(lash::TurnInput::text("still unbound"))
        .await
        .unwrap();
    // SQLite runs outside Tokio: prevent automatic time advancement
    // while a status read is in flight. Gates prove those reads finished.
    let hold_time = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    {
        let mut following = std::pin::pin!(handle.outcome());
        first.reached_by(&mut following, 1).await;
        let started = tokio::time::Instant::now();
        first.open_one();
        assert!(futures_util::poll!(&mut following).is_pending());
        tokio::time::advance(pacing.initial()).await;
        second.reached_by(&mut following, 1).await;
        second.open_one();
        assert!(futures_util::poll!(&mut following).is_pending());
        // No store work remains until the third gate (before its read).
        // Tokio can now advance directly to the next probe's timer.
        hold_time.abort();
        let _ = hold_time.await;
        third.reached_by(&mut following, 1).await;
        assert_eq!(
            started.elapsed(),
            pacing.initial() * 2,
            "unbound reads must not back off toward the 120-second ceiling"
        );
        third.open_one();
    }
    reads.finish().unwrap();
    core.shutdown().await.unwrap();
}
