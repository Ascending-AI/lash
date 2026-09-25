use super::*;

/// native-substrate end to end across the process wait, observation, and retention
/// interfaces: a host starts a process, holds `ProcessWorkSubstrate::await_process_terminal`
/// (through `core.processes().await_output`), signals it to completion, and
/// observes its intermediate events through a wired `ProcessEventSink` — then
/// prunes the terminal registry rows while the host's projected copies survive.
#[tokio::test]
async fn native_process_await_sink_and_prune_end_to_end() -> Result<()> {
    let backend = memory_backend().await;
    let artifact_store = lash_lashlang_runtime::LashlangArtifacts::new(backend.process_env_store());
    let registry: Arc<dyn lash_core::ProcessRegistry> = backend.process_registry();
    let sink = CollectingProcessEventSink::default();
    let core = process_test_core_with_sink(backend.clone(), Arc::new(sink.clone()))?;
    let process = LinkedTestProcess::new(
        &artifact_store,
        // process main() signals { ready: any } {
        //   value = wait_signal("ready")
        //   finish value
        // }
        wait_signal_process(lashlang::TypeExpr::Any, b::var("value")),
        "main",
    )
    .await;

    let process_id = "e2e-await-sink-prune";
    core.processes()
        .start(
            process.start_request(&ProcessId::from(process_id)),
            runtime_operation_scope(&core, "e2e-start"),
        )
        .await?;
    wait_for_waiting_signal(&core, &ProcessId::from(process_id), "ready").await;

    // Hold the terminal await while the process is still running; it must resolve
    // only once the signal drives the process to finish.
    let await_core = core.clone();
    let await_id = process_id.to_string();
    let started = std::time::Instant::now();
    let waiter = tokio::spawn(async move {
        await_core
            .processes()
            .await_output(&ProcessId::from(await_id))
            .await
    });

    let payload = serde_json::json!({ "ok": true, "answer": 42 });
    core.processes()
        .signal(
            &ProcessId::from(process_id),
            "ready",
            "e2e-signal-1",
            signal_request(
                &ProcessId::from(process_id),
                "ready",
                "e2e-signal-1",
                payload.clone(),
            ),
            runtime_operation_scope(&core, "e2e-signal"),
        )
        .await?;

    let output = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
        .await
        .expect("held await_terminal resolves within bound")
        .expect("join await task")?;
    let elapsed = started.elapsed();
    let output = output.into_tool_output();
    let lash_core::ToolCallOutcome::Success(value) = output.outcome else {
        panic!("process did not succeed: {output:#?}");
    };
    let value = value.to_json_value();
    assert_eq!(
        value, payload,
        "the held await_terminal yields exactly the process's finish value"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "the held await resolves promptly once the process completes (waited {elapsed:?})"
    );

    // The wired sink observed lifecycle, signal, and terminal events in append
    // order. The await seam remains authoritative for terminal observation:
    // the sink is pushed after the durable write, so the terminal push may
    // trail the awaiter by a moment.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !sink
            .collected()
            .iter()
            .any(|(event_type, _)| event_type == "process.completed")
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the sink observes the terminal append promptly");
    let collected = sink.collected();
    let sequences: Vec<u64> = collected.iter().map(|(_, sequence)| *sequence).collect();
    let mut sorted = sequences.clone();
    sorted.sort_unstable();
    assert_eq!(
        sequences, sorted,
        "the sink observes appended events in per-process append order; got {collected:?}"
    );
    assert!(
        collected
            .iter()
            .any(|(event_type, _)| event_type == "signal.ready"),
        "the sink observed the intermediate signal event; got {collected:?}"
    );
    assert!(
        collected
            .iter()
            .any(|(event_type, _)| event_type == "process.completed"),
        "the sink observed the terminal append; got {collected:?}"
    );

    wait_for_terminal(
        &core,
        &ProcessId::from(process_id),
        lash_core::ProcessStatus::Completed,
    )
    .await;

    // Retention: prune the terminal registry rows. The registry forgets the
    // process, but the host's projected copies (the sink log) remain intact.
    let projected_before_prune = sink.collected();
    let report = core
        .processes()
        .prune(
            i64::MAX as u64,
            None,
            lash_core::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune terminal process");
    assert_eq!(
        report.pruned_processes, 1,
        "the single terminal process is pruned"
    );
    assert!(
        matches!(
            registry.get_process(&ProcessId::from(process_id)).await,
            Err(lash_core::PluginError::ProcessNoLongerRetained { .. })
        ),
        "the pruned process returns the typed retained-history miss"
    );
    assert_eq!(
        sink.collected(),
        projected_before_prune,
        "the host's projected copies survive the registry prune untouched"
    );
    assert!(
        sink.collected()
            .iter()
            .any(|(event_type, _)| event_type == "signal.ready"),
        "the projected intermediate events remain available to the host after prune"
    );

    Ok(())
}
