use super::*;

pub(super) async fn run_engine_promise_conformance(
    admin_url: &str,
    ingress_url: &str,
) -> Result<()> {
    let key_host = RestateEffectHost::new(ingress_url.to_string(), restate_authority_id()?);
    let attached_key = engine_conformance_key(&key_host, "waiter-before-resolution").await?;
    let attached_expected = Resolution::Ok(json!({ "ordering": "waiter-before-resolution" }));
    let attached_wait_key = attached_key.clone();
    let attached_waiter =
        tokio::spawn(
            async move { await_durable_wait_on_worker("worker-a", attached_wait_key).await },
        );
    wait_for_durable_wait_suspended(admin_url, &attached_key).await?;
    let attached_outcome =
        resolve_durable_wait_from_peer_worker("worker-a", &attached_key, attached_expected.clone())
            .await?;
    anyhow::ensure!(
        matches!(attached_outcome, lash_core::ResolveOutcome::Accepted),
        "engine conformance attached resolution was not accepted: {attached_outcome:?}"
    );
    let attached_response = tokio::time::timeout(Duration::from_secs(30), attached_waiter)
        .await
        .context("attached engine conformance waiter did not wake")?
        .context("join attached engine conformance waiter")??;
    anyhow::ensure!(
        attached_response.worker_id == "worker-a"
            && attached_response.resolution == attached_expected,
        "attached engine conformance response mismatch: {attached_response:?}"
    );

    let pre_resolved_key = engine_conformance_key(&key_host, "resolution-before-waiter").await?;
    let pre_resolved_expected = Resolution::Ok(json!({ "ordering": "resolution-before-waiter" }));
    let pre_resolved_outcome = resolve_durable_wait_from_peer_worker(
        "worker-a",
        &pre_resolved_key,
        pre_resolved_expected.clone(),
    )
    .await?;
    anyhow::ensure!(
        matches!(pre_resolved_outcome, lash_core::ResolveOutcome::Accepted),
        "engine conformance pre-resolution was not accepted: {pre_resolved_outcome:?}"
    );
    let pre_resolved_response = tokio::time::timeout(
        Duration::from_secs(30),
        await_durable_wait_on_worker("worker-a", pre_resolved_key),
    )
    .await
    .context("pre-resolved engine conformance waiter did not return")??;
    anyhow::ensure!(
        pre_resolved_response.worker_id == "worker-a"
            && pre_resolved_response.resolution == pre_resolved_expected,
        "pre-resolved engine conformance response mismatch: {pre_resolved_response:?}"
    );
    println!(
        "promise engine conformance passed: suspended waiter=worker-a; resolver=worker-b; waiter-before-resolution; resolution-before-waiter"
    );
    Ok(())
}

pub(super) async fn resolve_durable_wait_from_peer_worker(
    waiter_worker_id: &str,
    key: &AwaitEventKey,
    resolution: Resolution,
) -> Result<lash_core::ResolveOutcome> {
    let resolver_worker_id = match waiter_worker_id {
        "worker-a" => "worker-b",
        "worker-b" => "worker-a",
        other => anyhow::bail!("durable waiter ran on unexpected worker `{other}`"),
    };
    wait_for_worker_control_healthy(resolver_worker_id).await?;
    let response = reqwest::Client::new()
        .post(format!(
            "http://{resolver_worker_id}:18101/resolve-durable-wait"
        ))
        .json(&DirectDurableWaitResolveRequest {
            key: key.clone(),
            resolution,
        })
        .send()
        .await
        .with_context(|| format!("ask `{resolver_worker_id}` to resolve durable wait"))?
        .error_for_status()
        .with_context(|| format!("`{resolver_worker_id}` durable-wait resolve status"))?
        .json::<DirectDurableWaitResolveResponse>()
        .await
        .with_context(|| format!("decode `{resolver_worker_id}` durable-wait resolve response"))?;
    anyhow::ensure!(
        response.worker_id == resolver_worker_id,
        "durable wait resolved on `{}` instead of peer `{resolver_worker_id}`",
        response.worker_id
    );
    eprintln!(
        "promise conformance: waiter={waiter_worker_id} resolver={} outcome={:?}",
        response.worker_id, response.outcome
    );
    Ok(response.outcome)
}

pub(super) async fn wait_for_worker_control_healthy(worker_id: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let url = format!("http://{worker_id}:18101/health");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last_error = None;
    while Instant::now() < deadline {
        match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => last_error = Some(format!("HTTP {}", response.status())),
            Err(err) => last_error = Some(err.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!(
        "worker control endpoint `{worker_id}` did not recover before peer resolution; last error={last_error:?}"
    )
}

pub(super) async fn drive_turn_control_scenarios(
    storage: &PostgresStorage,
    ingress_url: &str,
) -> Result<()> {
    let deployment = RestateTurnDeployment::new(ingress_url.to_string(), restate_authority_id()?);
    let driver = deployment.turn_work_driver(Arc::new(storage.session_store_factory()));

    let completed = TurnRequest {
        workflow_id: "e2e-turn-cancel-late-normal".to_string(),
        fail_once: false,
        scenario: TurnScenario::TurnControlComplete,
        signal: None,
    };
    submit_workflow(ingress_url, &completed).await?;
    let _ = wait_for_terminal_result(storage.pool(), &completed.workflow_id).await?;
    let completed_address = turn_address(&completed).await?;
    let completed_terminal = driver
        .await_terminal(&completed_address)
        .await
        .context("attach to normally completed turn")?;
    assert_non_cancel_terminal(&completed_terminal)?;
    assert_late_cancel_is_noop(
        storage,
        &driver,
        &completed_address,
        &completed_terminal,
        "e2e-cancel-late-normal",
    )
    .await?;

    // A remote host can durably win the gate before any worker owns the turn.
    let before = turn_control_request("e2e-turn-cancel-before-start", false);
    let before_evidence_id = "e2e-cancel-before-start";
    let before_outcome = driver
        .request_cancel(cancel_request(
            turn_address(&before).await?,
            before_evidence_id,
        ))
        .await
        .context("request cancellation before turn start")?;
    assert_requested(&before_outcome.outcome, before_evidence_id)?;
    submit_workflow(ingress_url, &before).await?;
    let before_terminal = driver
        .await_terminal(&turn_address(&before).await?)
        .await
        .context("attach to cancel-before-start terminal")?;
    report_workflow_progress(&before.workflow_id, "terminal-attached");
    assert_cancelled_terminal(&before_terminal, before_evidence_id)?;
    assert_cancelled_response(
        &wait_for_terminal_result(storage.pool(), &before.workflow_id).await?,
        before_evidence_id,
    )?;

    // The runner owns no Lash session handle. It waits for the peer worker's
    // tool boundary, then addresses the durable gate directly.
    let cross = turn_control_request("e2e-turn-cancel-cross-process", false);
    submit_workflow(ingress_url, &cross).await?;
    wait_for_cancel_gate(storage.pool(), &cross.workflow_id).await?;
    let cross_evidence_id = "e2e-cancel-cross-process";
    let cross_outcome = driver
        .request_cancel(cancel_request(
            turn_address(&cross).await?,
            cross_evidence_id,
        ))
        .await
        .context("request cross-process cancellation")?;
    assert_requested(&cross_outcome.outcome, cross_evidence_id)?;
    let cross_terminal = driver
        .await_terminal(&turn_address(&cross).await?)
        .await
        .context("attach to cross-process terminal")?;
    report_workflow_progress(&cross.workflow_id, "terminal-attached");
    assert_cancelled_terminal(&cross_terminal, cross_evidence_id)?;
    assert_cancelled_response(
        &wait_for_terminal_result(storage.pool(), &cross.workflow_id).await?,
        cross_evidence_id,
    )?;

    // This live gate races workflow submission against cancellation. The
    // tighter already-running commit-time seal window is covered by the inline
    // unit race; either way, a requested cancel must commit Cancelled with the
    // same evidence, while a completion seal must commit a non-cancel terminal
    // without evidence.
    let race = TurnRequest {
        workflow_id: "e2e-turn-cancel-seal-race".to_string(),
        fail_once: false,
        scenario: TurnScenario::TurnControlComplete,
        signal: None,
    };
    let race_evidence_id = "e2e-cancel-seal-race";
    let race_cancel = cancel_request(turn_address(&race).await?, race_evidence_id);
    let (submitted, race_outcome) = tokio::join!(
        submit_workflow(ingress_url, &race),
        driver.request_cancel(race_cancel),
    );
    submitted.context("submit completion/cancel race")?;
    let race_outcome = race_outcome.context("request completion/cancel race")?;
    let race_terminal = driver
        .await_terminal(&turn_address(&race).await?)
        .await
        .context("attach to completion/cancel race terminal")?;
    report_workflow_progress(&race.workflow_id, "terminal-attached");
    match race_outcome.outcome {
        TurnCancelOutcome::Requested(_)
        | TurnCancelOutcome::AlreadyRequested(_)
        | TurnCancelOutcome::Escalated(_) => {
            assert_cancelled_terminal(&race_terminal, race_evidence_id)?;
        }
        TurnCancelOutcome::CompletionWonRace => assert_non_cancel_terminal(&race_terminal)?,
        TurnCancelOutcome::UnknownOrRevoked => {
            anyhow::bail!("completion/cancel race unexpectedly targeted a revoked gate")
        }
        TurnCancelOutcome::PolicyConflict { accepted, .. } => {
            anyhow::bail!(
                "completion/cancel race unexpectedly found a conflicting accepted policy: {accepted:?}"
            )
        }
    }
    let _ = wait_for_terminal_result(storage.pool(), &race.workflow_id).await?;

    // The first owner exits from crash_once. Cancellation lands while Restate
    // is recovering the invocation; the peer owner must replay the keyed gate.
    let recovery = turn_control_request("e2e-turn-cancel-crash-recovery", true);
    submit_workflow(ingress_url, &recovery).await?;
    let _crashed_worker = wait_for_failover_marker(storage.pool(), &recovery.workflow_id).await?;
    let recovery_evidence_id = "e2e-cancel-crash-recovery";
    let recovery_outcome = driver
        .request_cancel(cancel_request(
            turn_address(&recovery).await?,
            recovery_evidence_id,
        ))
        .await
        .context("request cancellation during owner recovery")?;
    assert_requested(&recovery_outcome.outcome, recovery_evidence_id)?;
    report_workflow_progress(&recovery.workflow_id, "cancel-requested-after-crash");
    let recovery_terminal = driver
        .await_terminal(&turn_address(&recovery).await?)
        .await
        .context("attach to recovered cancellation terminal")?;
    report_workflow_progress(&recovery.workflow_id, "terminal-attached");
    assert_cancelled_terminal(&recovery_terminal, recovery_evidence_id)?;
    let recovery_response = wait_for_terminal_result(storage.pool(), &recovery.workflow_id).await?;
    assert_cancelled_response(&recovery_response, recovery_evidence_id)?;
    assert_late_cancel_is_noop(
        storage,
        &driver,
        &turn_address(&recovery).await?,
        &recovery_terminal,
        "e2e-cancel-late-after-recovery",
    )
    .await?;
    // FIG-1671 cede semantics (ADR 0069 §5(d)): the witness here is convergence,
    // not migration. This gate used to require a *different* worker to finish the
    // recovered turn, which encoded the pre-acceptance failover model where the
    // work had to move to a live peer. Under durable acceptance the loser of the
    // head CAS cedes — it writes nothing durable and never re-commits under
    // another authority — and liveness comes from the engine re-invoking against
    // the journaled acceptance. Which worker serves that re-invocation is the
    // engine's business, and it is routinely the restarted original one, because
    // the dead holder's session-execution lease has to age out first: the
    // recovered turn takes roughly one lease TTL (~38s) to settle. That latency
    // is understood and settled, not accidental: ADR 0080 rejects letting a
    // substrate that already guarantees invocation exclusivity short-circuit the
    // lease TTL, and leaves failover latency where ADR 0014 put it — the host's
    // `LeaseTimings`. What must hold is stricter than
    // the old identity check: the turn completes exactly once, against exactly one
    // acceptance, with no duplicate or conflicting settlement anywhere.
    assert_recovered_turn_converged(storage.pool(), &TurnId::from(recovery.workflow_id)).await?;

    println!(
        "turn-control gates passed: cross-process; cancel-before-start; seal-vs-cancel; owner-crash-recovery; terminal-attach-evidence; exact-address-late-noop"
    );
    Ok(())
}

async fn assert_late_cancel_is_noop(
    storage: &PostgresStorage,
    driver: &TurnWorkDriver,
    address: &TurnAddress,
    terminal: &TurnTerminal,
    request_id: &str,
) -> Result<()> {
    use lash_core::SessionStoreFactory as _;

    let store = storage
        .session_store_factory()
        .open_existing_store_by_id(&address.session_id)
        .await
        .map_err(anyhow::Error::msg)
        .context("open exact session for late-cancel proof")?
        .context("late-cancel session disappeared")?;
    let record_before = store
        .turn_cancel_request(address)
        .await
        .context("read cancellation record before late request")?;
    let active_before = store
        .list_pending_turn_inputs(&address.session_id)
        .await
        .context("list active inputs before late request")?
        .into_iter()
        .filter(|input| input.ingress.active_turn_id() == Some(&address.turn_id))
        .count();
    anyhow::ensure!(
        active_before == 0,
        "completed address was active before late request: {address:?}"
    );

    let late = driver
        .request_cancel(cancel_request(address.clone(), request_id))
        .await
        .context("repeat exact-address cancellation after terminal")?;
    anyhow::ensure!(
        matches!(late.outcome, TurnCancelOutcome::CompletionWonRace),
        "late exact-address cancellation was not a typed no-op: {:?}",
        late.outcome
    );
    anyhow::ensure!(
        late.record.is_none(),
        "late no-op returned a mutable record"
    );
    let terminal_after = driver
        .await_terminal(address)
        .await
        .context("reattach terminal after late request")?;
    anyhow::ensure!(
        serde_json::to_value(&terminal_after)? == serde_json::to_value(terminal)?,
        "late request changed the published terminal"
    );
    anyhow::ensure!(
        store
            .turn_cancel_request(address)
            .await
            .context("read cancellation record after late request")?
            == record_before,
        "late request changed durable cancellation evidence or disposition"
    );
    let active_after = store
        .list_pending_turn_inputs(&address.session_id)
        .await
        .context("list active inputs after late request")?
        .into_iter()
        .filter(|input| input.ingress.active_turn_id() == Some(&address.turn_id))
        .count();
    anyhow::ensure!(
        active_after == 0,
        "late request reopened the completed address: {address:?}"
    );
    Ok(())
}

/// Prove cancellation wakes a timer only after Restate reports the parent turn
/// suspended; the 300-second deadline is intentionally far outside the gate.
pub(super) async fn drive_suspended_sleep_cancel_scenario(
    storage: &PostgresStorage,
    ingress_url: &str,
    admin_url: &str,
) -> Result<()> {
    let request = TurnRequest {
        workflow_id: "e2e-suspended-sleep-cancel".to_string(),
        fail_once: false,
        scenario: TurnScenario::TurnControlSleep,
        signal: None,
    };
    let invocation_id = submit_workflow(ingress_url, &request).await?;
    let admin = RestateAdminClient::new(admin_url.to_string());
    wait_for_invocation_suspended(&admin, &invocation_id, Duration::from_secs(90)).await?;
    report_workflow_progress(&request.workflow_id, "durable-sleep-suspended");

    let evidence_id = "e2e-cancel-suspended-sleep";
    let driver = RestateTurnDeployment::new(ingress_url.to_string(), restate_authority_id()?)
        .turn_work_driver(Arc::new(storage.session_store_factory()));
    let started = Instant::now();
    let receipt = driver
        .request_cancel(cancel_request(turn_address(&request).await?, evidence_id))
        .await
        .context("request cancellation after durable sleep suspended")?;
    assert_requested(&receipt.outcome, evidence_id)?;
    let terminal = tokio::time::timeout(
        Duration::from_secs(10),
        driver.await_terminal(&turn_address(&request).await?),
    )
    .await
    .context("suspended durable sleep did not wake within 10 seconds")??;
    assert_cancelled_terminal(&terminal, evidence_id)?;
    anyhow::ensure!(
        started.elapsed() < Duration::from_secs(10),
        "suspended sleep cancellation waited for the 300-second timer"
    );
    let response = wait_for_terminal_result(storage.pool(), &request.workflow_id).await?;
    assert_cancelled_response(&response, evidence_id)?;
    println!("suspended-sleep gates passed: post-suspension-cancel");
    Ok(())
}

/// Coordinate a real Restate service bounce with the shell harness while both
/// workers and this runner stay alive. The tool wait preserves the existing
/// start-gate replay proof, while the timer proves a resolved cancel promise
/// wakes a suspended parent after the engine returns.
pub(super) async fn drive_engine_restart_scenario(
    storage: &PostgresStorage,
    ingress_url: &str,
    admin_url: &str,
) -> Result<()> {
    let driver = RestateTurnDeployment::new(ingress_url.to_string(), restate_authority_id()?)
        .turn_work_driver(Arc::new(storage.session_store_factory()));
    let parked = turn_control_request("e2e-engine-restart-cancel", false);
    submit_workflow(ingress_url, &parked).await?;
    let sleeping = TurnRequest {
        workflow_id: "e2e-engine-restart-suspended-sleep".to_string(),
        fail_once: false,
        scenario: TurnScenario::TurnControlSleep,
        signal: None,
    };
    let sleeping_invocation_id = submit_workflow(ingress_url, &sleeping).await?;
    let admin = RestateAdminClient::new(admin_url.to_string());
    wait_for_cancel_gate_attempts(storage.pool(), &parked.workflow_id, 1).await?;
    wait_for_invocation_suspended(&admin, &sleeping_invocation_id, Duration::from_secs(90)).await?;
    record_harness_signal(storage.pool(), "engine-restart-ready").await?;
    report_workflow_progress(&parked.workflow_id, "parked-before-engine-restart");

    wait_for_harness_signal(storage.pool(), "engine-restart-complete").await?;
    wait_for_restate_recovery(admin_url).await?;
    wait_for_cancel_gate_attempts(storage.pool(), &parked.workflow_id, 2).await?;
    wait_for_invocation_suspended(&admin, &sleeping_invocation_id, Duration::from_secs(30)).await?;
    report_workflow_progress(&parked.workflow_id, "journal-replayed-after-engine-restart");

    let sleep_evidence_id = "e2e-cancel-suspended-sleep-after-engine-restart";
    let sleep_cancel_started = Instant::now();
    let sleep_receipt = driver
        .request_cancel(cancel_request(
            turn_address(&sleeping).await?,
            sleep_evidence_id,
        ))
        .await
        .context("request suspended sleep cancellation after Restate engine restart")?;
    assert_requested(&sleep_receipt.outcome, sleep_evidence_id)?;
    let sleep_terminal = tokio::time::timeout(
        Duration::from_secs(10),
        driver.await_terminal(&turn_address(&sleeping).await?),
    )
    .await
    .context("post-restart suspended sleep did not wake within 10 seconds")??;
    assert_cancelled_terminal(&sleep_terminal, sleep_evidence_id)?;
    anyhow::ensure!(
        sleep_cancel_started.elapsed() < Duration::from_secs(10),
        "post-restart suspended sleep cancellation waited for the 300-second timer"
    );
    let sleep_response = wait_for_terminal_result(storage.pool(), &sleeping.workflow_id).await?;
    assert_cancelled_response(&sleep_response, sleep_evidence_id)?;

    let evidence_id = "e2e-cancel-after-engine-restart";
    let parked_address = turn_address(&parked).await?;
    let receipt = driver
        .request_cancel(
            TurnCancelRequest::new(
                parked_address.clone(),
                evidence_id,
                Some("scripted-engine-restart-runner".to_string()),
            )
            .with_reason("cancel a replayed turn after the Restate engine restarted"),
        )
        .await
        .context("request cancellation after Restate engine restart")?;
    let evidence = match &receipt.outcome {
        TurnCancelOutcome::Requested(evidence) | TurnCancelOutcome::AlreadyRequested(evidence) => {
            evidence
        }
        other => anyhow::bail!("post-restart cancellation did not win: {other:?}"),
    };
    anyhow::ensure!(
        evidence.request_id == evidence_id
            && evidence.origin.as_deref() == Some("scripted-engine-restart-runner")
            && evidence.reason.as_deref()
                == Some("cancel a replayed turn after the Restate engine restarted"),
        "post-restart cancellation receipt lost evidence: {evidence:?}"
    );
    let terminal = driver
        .await_terminal(&parked_address)
        .await
        .context("attach to post-restart cancellation terminal")?;
    assert_engine_restart_cancelled_terminal(&terminal, evidence_id)?;
    let cancelled = wait_for_terminal_result(storage.pool(), &parked.workflow_id).await?;
    anyhow::ensure!(
        cancelled.final_text == "turn-control-cancelled"
            && cancelled.final_value["cancellation"]["request_id"] == evidence_id
            && cancelled.final_value["cancellation"]["origin"] == "scripted-engine-restart-runner",
        "post-restart worker result lost cancellation evidence: {cancelled:#?}"
    );

    let complete = TurnRequest {
        workflow_id: "e2e-engine-restart-complete".to_string(),
        fail_once: false,
        scenario: TurnScenario::TurnControlComplete,
        signal: None,
    };
    submit_workflow(ingress_url, &complete).await?;
    let complete_terminal = driver
        .await_terminal(&turn_address(&complete).await?)
        .await
        .context("attach to post-restart completion terminal")?;
    assert_non_cancel_terminal(&complete_terminal)?;
    let completed = wait_for_terminal_result(storage.pool(), &complete.workflow_id).await?;
    anyhow::ensure!(
        completed.final_text == "turn-control-completed",
        "post-restart turn did not commit normally: {completed:#?}"
    );
    println!(
        "engine-restart gates passed: journal-replay; suspended-sleep-cancel; post-restart-cancel-evidence; post-restart-completion"
    );
    Ok(())
}

pub(super) async fn record_harness_signal(pool: &sqlx::PgPool, signal_name: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO lash_e2e_harness_signals (signal_name, created_at_ms)
         VALUES ($1, (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT)
         ON CONFLICT (signal_name) DO UPDATE SET created_at_ms = EXCLUDED.created_at_ms",
    )
    .bind(signal_name)
    .execute(pool)
    .await
    .with_context(|| format!("record harness signal `{signal_name}`"))?;
    Ok(())
}

pub(super) async fn wait_for_harness_signal(pool: &sqlx::PgPool, signal_name: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let seen: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM lash_e2e_harness_signals WHERE signal_name = $1)",
        )
        .bind(signal_name)
        .fetch_one(pool)
        .await
        .with_context(|| format!("poll harness signal `{signal_name}`"))?;
        if seen {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    anyhow::bail!("timed out waiting for harness signal `{signal_name}`")
}

pub(super) async fn wait_for_restate_recovery(admin_url: &str) -> Result<()> {
    let admin = RestateAdminClient::new(admin_url.to_string());
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        if admin
            .unfinished_invocations_for_service_prefixes(&[TURN_WORKFLOW_NAME])
            .await
            .is_ok()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    anyhow::bail!("Restate admin API did not recover after engine restart")
}

pub(super) fn assert_engine_restart_cancelled_terminal(
    terminal: &TurnTerminal,
    request_id: &str,
) -> Result<()> {
    let TurnTerminal::Committed {
        outcome: TurnOutcome::Stopped(TurnStop::Cancelled { evidence }),
        ..
    } = terminal
    else {
        anyhow::bail!("expected committed post-restart cancellation, got {terminal:?}")
    };
    anyhow::ensure!(
        evidence.request_id == request_id
            && evidence.origin.as_deref() == Some("scripted-engine-restart-runner")
            && evidence.reason.as_deref()
                == Some("cancel a replayed turn after the Restate engine restarted"),
        "post-restart terminal lost cancellation evidence: {terminal:?}"
    );
    Ok(())
}

pub(super) async fn drive_break_glass_scenario(
    storage: &PostgresStorage,
    ingress_url: &str,
    admin_url: &str,
) -> Result<()> {
    // Run this last: a hard-killed handler cannot release its shared-session
    // execution lease, and no subsequent scenario should depend on that lease.
    // This remains a negative operator gate and must not manufacture a Lash
    // Cancelled terminal. Graceful Restate cancellation cannot interrupt
    // arbitrary local user code blocked inside a running side-effect closure.
    let break_glass = turn_control_request("e2e-turn-break-glass", false);
    let invocation_id = submit_workflow(ingress_url, &break_glass).await?;
    wait_for_cancel_gate(storage.pool(), &break_glass.workflow_id).await?;
    let admin = RestateAdminClient::new(admin_url.to_string());
    admin
        .kill_invocation_for_test_cleanup(&invocation_id)
        .await
        .context("kill Restate invocation as break-glass")?;
    report_workflow_progress(&break_glass.workflow_id, "admin-kill-requested");
    wait_for_invocation_terminal(&admin, &invocation_id).await?;

    let driver = TurnWorkDriver::for_catalog(
        Arc::new(RestateEffectHost::new(
            ingress_url.to_string(),
            restate_authority_id()?,
        )),
        Arc::new(storage.session_store_factory()),
    );
    if let Ok(Ok(terminal)) = tokio::time::timeout(
        Duration::from_secs(3),
        driver.await_terminal(&turn_address(&break_glass).await?),
    )
    .await
    {
        anyhow::ensure!(
            !matches!(
                terminal,
                TurnTerminal::Committed {
                    outcome: TurnOutcome::Stopped(TurnStop::Cancelled { .. }),
                    ..
                }
            ),
            "break-glass invocation kill was reported as Lash cancellation"
        );
    }
    anyhow::ensure!(
        load_terminal_result(storage.pool(), &break_glass.workflow_id)
            .await?
            .is_none(),
        "break-glass invocation kill was reported as a Lash terminal result"
    );
    println!("break-glass gate passed: Restate hard-kill was not reported as Lash cancellation");
    Ok(())
}

pub(super) fn turn_control_request(workflow_id: &str, fail_once: bool) -> TurnRequest {
    TurnRequest {
        workflow_id: workflow_id.to_string(),
        fail_once,
        scenario: TurnScenario::TurnControlHold,
        signal: None,
    }
}

pub(super) async fn turn_address(request: &TurnRequest) -> Result<TurnAddress> {
    let session_id = turn_session_id(&request.workflow_id);
    Ok(TurnAddress::new(session_id, request.workflow_id.clone()))
}

pub(super) fn cancel_request(address: TurnAddress, request_id: &str) -> TurnCancelRequest {
    TurnCancelRequest::new(address, request_id, Some("scripted-e2e-runner".to_string()))
        .with_reason("deterministic Restate/Postgres workers E2E gate")
}

pub(super) fn assert_requested(outcome: &TurnCancelOutcome, request_id: &str) -> Result<()> {
    let evidence = match outcome {
        TurnCancelOutcome::Requested(evidence) => evidence,
        other => anyhow::bail!("expected cancellation request `{request_id}` to win: {other:?}"),
    };
    anyhow::ensure!(
        evidence.request_id == request_id,
        "cancellation receipt lost request evidence: {evidence:?}"
    );
    anyhow::ensure!(
        evidence.origin.as_deref() == Some("scripted-e2e-runner"),
        "cancellation receipt changed opaque host origin: {evidence:?}"
    );
    Ok(())
}

pub(super) fn assert_cancelled_terminal(terminal: &TurnTerminal, request_id: &str) -> Result<()> {
    let TurnTerminal::Committed {
        outcome,
        session_revision: _,
    } = terminal
    else {
        anyhow::bail!("expected committed cancellation terminal, got {terminal:?}")
    };
    let evidence = outcome
        .cancellation()
        .context("terminal was not Cancelled")?;
    anyhow::ensure!(
        evidence.request_id == request_id,
        "terminal evidence mismatch: {evidence:?}"
    );
    anyhow::ensure!(
        evidence.origin.as_deref() == Some("scripted-e2e-runner"),
        "terminal changed opaque host origin: {evidence:?}"
    );
    Ok(())
}

pub(super) fn assert_non_cancel_terminal(terminal: &TurnTerminal) -> Result<()> {
    let TurnTerminal::Committed { outcome, .. } = terminal else {
        anyhow::bail!("expected committed completion terminal, got {terminal:?}")
    };
    anyhow::ensure!(
        outcome.cancellation().is_none(),
        "completion-sealed terminal reported Cancelled"
    );
    Ok(())
}

pub(super) fn assert_cancelled_response(response: &TurnResponse, request_id: &str) -> Result<()> {
    anyhow::ensure!(
        response.final_text == "turn-control-cancelled",
        "worker did not record authoritative cancellation: {response:#?}"
    );
    anyhow::ensure!(
        response.final_value["cancellation"]["request_id"] == request_id,
        "recorded terminal lost cancellation evidence: {response:#?}"
    );
    anyhow::ensure!(
        response.final_value["cancellation"]["origin"] == "scripted-e2e-runner",
        "recorded terminal changed opaque host origin: {response:#?}"
    );
    Ok(())
}

pub(super) async fn wait_for_cancel_gate(pool: &sqlx::PgPool, workflow_id: &str) -> Result<()> {
    wait_for_cancel_gate_attempts(pool, workflow_id, 1).await
}

pub(super) async fn wait_for_cancel_gate_attempts(
    pool: &sqlx::PgPool,
    workflow_id: &str,
    expected: i64,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM lash_e2e_tool_events
             WHERE workflow_id = $1 AND tool_name = 'cancel_gate'",
        )
        .bind(workflow_id)
        .fetch_one(pool)
        .await
        .context("poll cancellation tool gate")?;
        if count >= expected {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    anyhow::bail!("timed out waiting for {expected} cancellation-gate attempts in `{workflow_id}`")
}

pub(super) async fn wait_for_failover_marker(
    pool: &sqlx::PgPool,
    workflow_id: &str,
) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let worker = sqlx::query_scalar::<_, String>(
            "SELECT worker_id FROM lash_e2e_failover_markers WHERE workflow_id = $1",
        )
        .bind(workflow_id)
        .fetch_optional(pool)
        .await
        .context("poll turn-control failover marker")?;
        if let Some(worker) = worker {
            return Ok(worker);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    anyhow::bail!("timed out waiting for owner crash in `{workflow_id}`")
}

/// Assert a turn recovered after an owner crash converged on the acceptance it
/// was journaled under, rather than being executed a second time.
///
/// The evidence is read from lash's own durable tables, not the harness's
/// mirror of them: the harness table is keyed by workflow id and would hide a
/// second completion behind an upsert. A turn that re-executed from scratch
/// would have to mint a fresh acceptance and commit it, which shows up here as
/// a second commit row, a second applied `input_id`, the same `input_id`
/// settled by another turn, or a row left unsettled in the pending queue.
pub(super) async fn assert_recovered_turn_converged(
    pool: &sqlx::PgPool,
    turn_id: &TurnId,
) -> Result<()> {
    // Scope every read to the session the workflow actually runs in, so the
    // helper stays correct for the scenarios that use their own session.
    let session_id = turn_session_id(turn_id);
    let commits = sqlx::query_as::<_, (String, String)>(
        "SELECT turn_id, result_json FROM lash_runtime_turn_commits WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await
    .context("load runtime turn commits for the recovered session")?;

    let mut commits_for_turn = 0usize;
    let mut applied_here: Vec<String> = Vec::new();
    let mut applied_elsewhere: Vec<String> = Vec::new();
    for (committed_scope, result_json) in &commits {
        // The commit key is the JSON execution scope, not a bare turn id.
        let committed_turn_id = serde_json::from_str::<serde_json::Value>(committed_scope)
            .ok()
            .and_then(|scope| {
                scope
                    .get("scope")
                    .and_then(|scope| scope.get("turn_id"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| committed_scope.clone());
        let receipt: serde_json::Value = serde_json::from_str(result_json)
            .with_context(|| format!("decode runtime commit receipt for `{committed_turn_id}`"))?;
        let applied = receipt
            .get("turn_input_applications")
            .and_then(serde_json::Value::as_array)
            .map(|applications| {
                applications
                    .iter()
                    .filter_map(|application| {
                        application
                            .get("input_id")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if committed_turn_id == turn_id.as_str() {
            commits_for_turn += 1;
            applied_here.extend(applied);
        } else {
            applied_elsewhere.extend(applied);
        }
    }

    anyhow::ensure!(
        commits_for_turn == 1,
        "recovered turn `{turn_id}` has {commits_for_turn} durable commits; ceding promises exactly one"
    );

    let mut accepted = applied_here.clone();
    accepted.sort();
    accepted.dedup();
    anyhow::ensure!(
        accepted.len() == applied_here.len(),
        "recovered turn `{turn_id}` settled the same input twice: {applied_here:?}"
    );
    anyhow::ensure!(
        accepted.len() <= 1,
        "recovered turn `{turn_id}` settled {} acceptances; a converged redrive redeems the one it was accepted under: {accepted:?}",
        accepted.len()
    );

    let conflicting = accepted
        .iter()
        .filter(|input_id| applied_elsewhere.contains(input_id))
        .cloned()
        .collect::<Vec<_>>();
    anyhow::ensure!(
        conflicting.is_empty(),
        "input(s) {conflicting:?} were settled by `{turn_id}` and by another turn in the same session"
    );

    // An empty application set is only legal for a turn that was cancelled before
    // any input was applied to it. No scenario routed through this helper is in
    // that shape - each one drives an accepted input to a durable commit - so an
    // empty set here means the receipt shape drifted (`turn_input_applications`
    // is `skip_serializing_if = "Vec::is_empty"`) and the checks below would
    // otherwise degrade to "one commit row exists".
    let accepted_input_id = accepted.first().with_context(|| {
        format!(
            "recovered turn `{turn_id}` committed without applying any acceptance; expected exactly one"
        )
    })?;
    let unsettled = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM lash_pending_turn_inputs
         WHERE session_id = $1 AND input_id = $2 AND state <> $3 AND state <> $4",
    )
    .bind(session_id)
    .bind(accepted_input_id)
    .bind(lash::persistence::TurnInputState::Completed.as_str())
    .bind(lash::persistence::TurnInputState::Cancelled.as_str())
    .fetch_one(pool)
    .await
    .context("count unsettled pending turn inputs for the recovered acceptance")?;
    anyhow::ensure!(
        unsettled == 0,
        "acceptance `{accepted_input_id}` is still unsettled in the pending queue after `{turn_id}` committed"
    );
    println!(
        "owner-crash recovery converged: `{turn_id}` committed exactly once against acceptance `{accepted_input_id}`, settled once, with no duplicate or conflicting settlement"
    );
    Ok(())
}

pub(super) async fn wait_for_invocation_terminal(
    admin: &RestateAdminClient,
    invocation_id: &RestateInvocationId,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        if admin
            .invocation_status(invocation_id)
            .await
            .context("read break-glass invocation status")?
            .is_some_and(|status| !status.is_still_active())
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    anyhow::bail!("break-glass invocation `{invocation_id}` did not terminate")
}

pub(super) async fn wait_for_invocation_suspended(
    admin: &RestateAdminClient,
    invocation_id: &RestateInvocationId,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let last_status = admin
            .invocation_status(invocation_id)
            .await
            .context("read Restate invocation suspension status")?;
        if last_status
            .as_ref()
            .is_some_and(|status| status.status == "suspended")
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "Restate invocation `{invocation_id}` did not suspend within {timeout:?}; last status={last_status:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
