use super::*;

/// The file backend an approval test runs on: its effect journal is the
/// durable host the parked approval resumes through.
async fn approval_backend(
    data_dir: &std::path::Path,
    clock: Option<Arc<lash::testing::TestClock>>,
) -> Arc<lash_sqlite_store::SqliteBackend> {
    let root = data_dir.join("lash-sessions");
    let backend = match clock {
        Some(clock) => {
            lash_sqlite_store::SqliteBackend::open_with_options_and_clock(
                &root,
                lash_sqlite_store::SqliteBackendOptions::default(),
                clock,
            )
            .await
        }
        None => lash_sqlite_store::SqliteBackend::open(&root).await,
    };
    Arc::new(backend.expect("open the approval test backend"))
}

async fn approval_test_core(
    backend: &Arc<lash_sqlite_store::SqliteBackend>,
    provider: ProviderHandle,
    approvals: approvals::WorkbenchApprovals,
) -> LashCore {
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .channel(lash::rlm::RlmChannel::Cell)
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build()
            .with_lashlang_abilities(workbench_lashlang_abilities()),
        &backend.clone().into(),
    );
    LashCore::rlm_builder(backend.clone().into(), lash::TurnBudget::Unbounded, factory)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .provider(provider)
        .session_spec(lash::SessionSpec::new().turn_budget(lash::TurnBudget::Unbounded))
        .model(test_model())
        // The `processes` module is catalogue presence, not an ability bit (ADR
        // 0095): the workbench's scripted sources author `processes.*`, so the
        // surface only exists when this factory is installed, as bootstrap does.
        .plugin(Arc::new(lash_plugin_process_controls::SessionProcessAdminPluginFactory::new()))
        .plugin(Arc::new(
            WorkbenchPluginFactory::new().with_approvals(approvals),
        ))
        .without_queued_work()
        .build(crate::test_core_owner())
        .expect("build approval test core")
}

async fn wait_for_approval(
    approvals: &approvals::WorkbenchApprovals,
    turn: &mut tokio::task::JoinHandle<lash::Result<lash::TurnOutput>>,
) -> approvals::PendingApproval {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            if let Some(approval) = approvals.pending().expect("list approvals").pop() {
                return approval;
            }
            tokio::select! {
                outcome = &mut *turn => panic!("turn completed before publishing approval: {outcome:?}"),
                () = tokio::task::yield_now() => {}
            }
        }
    })
    .await
    .expect("approval wait must be published")
}

#[test]
fn approval_approve_resumes_parked_lashlang_instruction_with_success() {
    run_async_test_on_stack_budget("workbench-approval-approve", || async {
        let directory = tempfile::tempdir().expect("approval tempdir");
        let approvals = approvals::WorkbenchApprovals::open(directory.path().join("approvals.db"))
            .expect("open approval ledger");
        let backend = approval_backend(directory.path(), None).await;
        let effect_host = backend.effect_host();
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-approval-approve")
            .complete(|_| async {
                Ok(text_response(
                    r#"<typescript>
const result = await ops.apply_change({ target: "demo-cluster", change: "enable safe mode" });
finish(result);
</typescript>"#,
                ))
            })
            .build()
            .into_handle();
        let core = approval_test_core(&backend, provider, approvals.clone()).await;
        let session = core
            .session("approval-approve")
            .open()
            .await
            .expect("open approval session");
        let turn_scope = lash::durability::EffectHost::scoped_static(
            effect_host.as_ref(),
            lash::runtime::AdmittedScope::turn("approval-approve", "approval-approve-turn"),
        )
        .expect("scope approval turn")
        .expect("durable approval scope");
        let mut turn = tokio::spawn(async move {
            session
                .turn(lash::TurnInput::text("Apply the demo change."))
                .turn_id("approval-approve-turn")
                .require_finish()
                .expect("require approval finish")
                .advanced()
                .run_with_scope(turn_scope)
                .await
        });
        let approval = wait_for_approval(&approvals, &mut turn).await;
        let key = approvals
            .completion_key(&approval.key)
            .expect("read completion key");
        assert_eq!(approval.tool, approvals::APPROVAL_TOOL_NAME);
        assert_eq!(approval.requesting_session, "approval-approve");
        assert_eq!(key.key_id, approval.key);
        let discovered = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let waits = core
                    .completions()
                    .outstanding(&lash::SessionId::from("approval-approve"))
                    .await
                    .expect("discover session waits");
                if waits.contains(&key) {
                    break waits;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("approval wait becomes registered");
        let discovered_approval = discovered
            .iter()
            .find(|candidate| *candidate == &key)
            .cloned()
            .expect("approval wait is discoverable among all session waits");
        assert!(discovered.iter().any(|candidate| matches!(
            &candidate.wait,
            lash::AwaitEventWaitIdentity::TurnCancelGate
        )));
        let accepted = core
            .completions()
            .resolve(
                discovered_approval,
                approvals::approval_resolution(&approval),
            )
            .await
            .expect("approve completion");
        assert_eq!(accepted, lash::ResolveOutcome::Accepted);
        let remaining = core
            .completions()
            .outstanding(&lash::SessionId::from("approval-approve"))
            .await
            .expect("settled approval is absent from wait discovery");
        assert!(!remaining.contains(&key));
        let already_resolved = core
            .completions()
            .resolve(key.clone(), approvals::approval_resolution(&approval))
            .await
            .expect("duplicate resolve");
        let duplicate = lash::ResolveOutcome::AlreadyResolved {
            terminal: approvals::approval_resolution(&approval),
        };
        assert_eq!(already_resolved, duplicate);
        let unknown_key = lash::AwaitEventKey {
            scope: lash::runtime::ExecutionScope::turn("approval-approve", "approval-approve-turn"),
            wait: lash::AwaitEventWaitIdentity::tool_completion("unknown-tool-call"),
            key_id: "unknown-key".to_string(),
            signature: "unknown-signature".to_string(),
        };
        let unknown_outcome = core
            .completions()
            .resolve(unknown_key, approvals::approval_resolution(&approval))
            .await
            .expect("unknown resolve");
        assert_eq!(unknown_outcome, lash::ResolveOutcome::UnknownOrRevoked);
        approvals
            .mark_decided(&approval.key, approvals::ApprovalDecision::Approved)
            .expect("settle approval row");
        let output = turn
            .await
            .expect("approval turn task")
            .expect("approval turn succeeds");
        assert_eq!(
            output.final_value(),
            Some(&json!({
                "status": "applied",
                "target": "demo-cluster",
                "change": "enable safe mode"
            }))
        );
        assert!(approvals.pending().unwrap().is_empty());
    });
}

/// FIG-3293: a crash between the ledger write and the completion resolve
/// leaves a decided row over an outstanding wait. The row no longer lists as
/// pending, so the retry — and the boot reconcile — must re-drive `resolve`
/// from the recorded decision instead of reporting "not pending".
#[test]
fn a_decided_but_unresolved_approval_repairs_on_retry() {
    run_async_test_on_stack_budget("workbench-approval-repair", || async {
        let directory = tempfile::tempdir().expect("approval tempdir");
        let approvals = approvals::WorkbenchApprovals::open(directory.path().join("approvals.db"))
            .expect("open approval ledger");
        let backend = approval_backend(directory.path(), None).await;
        let effect_host = backend.effect_host();
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-approval-repair")
            .complete(|_| async {
                Ok(text_response(
                    r#"<typescript>
const result = await ops.apply_change({ target: "demo-cluster", change: "enable safe mode" });
finish(result);
</typescript>"#,
                ))
            })
            .build()
            .into_handle();
        let core = approval_test_core(&backend, provider, approvals.clone()).await;
        let session = core
            .session("approval-repair")
            .open()
            .await
            .expect("open approval session");
        let turn_scope = lash::durability::EffectHost::scoped_static(
            effect_host.as_ref(),
            lash::runtime::AdmittedScope::turn("approval-repair", "approval-repair-turn"),
        )
        .expect("scope approval turn")
        .expect("durable approval scope");
        let mut turn = tokio::spawn(async move {
            session
                .turn(lash::TurnInput::text("Apply the demo change."))
                .turn_id("approval-repair-turn")
                .require_finish()
                .expect("require approval finish")
                .advanced()
                .run_with_scope(turn_scope)
                .await
        });
        let approval = wait_for_approval(&approvals, &mut turn).await;

        // The crash point under test: the ledger recorded the decision and the
        // completion resolve never ran.
        approvals
            .mark_decided(&approval.key, approvals::ApprovalDecision::Approved)
            .expect("record decision");
        assert!(
            approvals.pending().unwrap().is_empty(),
            "the decided row no longer lists as pending"
        );

        let process_registry = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::open(
                &directory
                    .path()
                    .join("lash-sessions")
                    .join("process-registry.db"),
                directory.path().join("processes-sessions"),
            )
            .await
            .expect("open process registry"),
        ) as Arc<dyn lash::process::ProcessRegistry>;
        let session_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(directory.path().join("sessions")),
        );
        let state = AppState {
            unknown_turn_terminals: UnknownTurnTerminals::default(),
            core,
            attachment_store: test_attachment_store(),
            session_store_factory,
            trigger_store: detached_trigger_store(),
            process_observer: lash::process::ProcessWorkObserver::new(process_registry),
            sessions: WorkbenchSessions::fresh(),
            messages: Arc::new(Mutex::new(Vec::new())),
            selected_model: Arc::new(Mutex::new(ModelSelection {
                model: "test-model".to_string(),
                model_variant: Default::default(),
            })),
            trace_sink: None,
            lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
            event_tx: SessionEventRegistry::new(16),
            queued_work_driver: inert_queued_work(),
            restate_ingress_url: "http://127.0.0.1:8080".to_string(),
            restate_admin_url: "http://127.0.0.1:9070".to_string(),
            restate_http: reqwest::Client::new(),
            restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
            mail_world: mail::MailWorld::new(),
            active_turns: ActiveTurns::default(),
            authorization: WorkbenchAuthorization::allow_all(),
            approvals: approvals.clone(),
        };

        let response = decide_approval(&state, &approval.key, true)
            .await
            .expect("the retry repairs the recorded decision");
        assert_eq!(response.0["decision"], json!("approved"));

        let output = turn
            .await
            .expect("approval turn task")
            .expect("approval turn succeeds");
        assert_eq!(
            output.final_value(),
            Some(&json!({
                "status": "applied",
                "target": "demo-cluster",
                "change": "enable safe mode"
            }))
        );
    });
}

#[test]
fn approval_denial_preserves_typed_failure_fields_through_lashlang_bridge() {
    run_async_test_on_stack_budget("workbench-approval-deny", || async {
        let directory = tempfile::tempdir().expect("approval tempdir");
        let approvals = approvals::WorkbenchApprovals::open(directory.path().join("approvals.db"))
            .expect("open approval ledger");
        let backend = approval_backend(directory.path(), None).await;
        let effect_host = backend.effect_host();
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-approval-deny")
            .complete(|_| async {
                Ok(text_response(
                    r#"<typescript>
try {
  const value = await ops.apply_change({ target: "demo-cluster", change: "disable audit log" });
  finish({ ok: true, value: value });
} catch (error) {
  finish({ ok: false, error: error.message, cause: error.cause });
}
</typescript>"#,
                ))
            })
            .build()
            .into_handle();
        let core = approval_test_core(&backend, provider, approvals.clone()).await;
        let session = core
            .session("approval-deny")
            .open()
            .await
            .expect("open denial session");
        let turn_scope = lash::durability::EffectHost::scoped_static(
            effect_host.as_ref(),
            lash::runtime::AdmittedScope::turn("approval-deny", "approval-deny-turn"),
        )
        .expect("scope denial turn")
        .expect("durable denial scope");
        let mut turn = tokio::spawn(async move {
            session
                .turn(lash::TurnInput::text("Apply the demo change."))
                .turn_id("approval-deny-turn")
                .require_finish()
                .expect("require denial finish")
                .advanced()
                .run_with_scope(turn_scope)
                .await
        });
        let approval = wait_for_approval(&approvals, &mut turn).await;
        assert_eq!(
            core.completions()
                .resolve(
                    approvals.completion_key(&approval.key).unwrap(),
                    approvals::denial_resolution(),
                )
                .await
                .expect("deny completion"),
            lash::ResolveOutcome::Accepted
        );
        approvals
            .mark_decided(&approval.key, approvals::ApprovalDecision::Denied)
            .expect("settle denial row");
        let output = turn
            .await
            .expect("denial turn task")
            .expect("denial is handled in Lashlang");
        let final_value = output.final_value().expect("denial wrapper");
        assert_eq!(final_value.get("ok"), Some(&Value::Bool(false)));
        assert_eq!(
            final_value.get("error"),
            Some(&json!("the operator denied this change"))
        );
        let typed_failure = final_value
            .get("cause")
            .expect("Lashlang bridge preserves typed tool failure fields");
        assert_eq!(typed_failure.get("class"), Some(&json!("execution")));
        assert_eq!(
            typed_failure.get("code"),
            Some(&json!("agent_workbench:approval_denied"))
        );
        assert_eq!(typed_failure.get("source"), Some(&json!("tool")));
        assert_eq!(typed_failure["retry"]["type"], "never");
    });
}

#[test]
fn approval_restart_reopens_the_ledger_and_durable_effect_host() {
    run_async_test_on_stack_budget("workbench-approval-restart", || async {
        let directory = tempfile::tempdir().expect("approval tempdir");
        let approval_path = directory.path().join("approvals.db");
        let approvals =
            approvals::WorkbenchApprovals::open(&approval_path).expect("open approval ledger");
        let backend = approval_backend(directory.path(), None).await;
        let effect_host = backend.effect_host();
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-approval-restart")
            .complete(|_| async {
                Ok(text_response(
                    r#"<typescript>
const result = await ops.apply_change({ target: "restart-demo", change: "rotate workers" });
finish(result.status);
</typescript>"#,
                ))
            })
            .build()
            .into_handle();
        let core = approval_test_core(&backend, provider, approvals.clone()).await;
        let session = core
            .session("approval-restart")
            .open()
            .await
            .expect("open restart session");
        let turn_scope = lash::durability::EffectHost::scoped_static(
            effect_host.as_ref(),
            lash::runtime::AdmittedScope::turn("approval-restart", "approval-restart-turn"),
        )
        .expect("scope restart turn")
        .expect("durable restart scope");
        let mut turn = tokio::spawn(async move {
            session
                .turn(lash::TurnInput::text("Apply the restart demo change."))
                .turn_id("approval-restart-turn")
                .require_finish()
                .expect("require restart finish")
                .advanced()
                .run_with_scope(turn_scope)
                .await
        });
        let before_restart = wait_for_approval(&approvals, &mut turn).await;

        let reopened_approvals = approvals::WorkbenchApprovals::open(&approval_path)
            .expect("reopen approval ledger after process loss");
        let after_restart = reopened_approvals
            .pending()
            .expect("list approval after reopen")
            .pop()
            .expect("parked approval survives reopen");
        assert_eq!(after_restart.key, before_restart.key);
        assert_eq!(after_restart.arguments, before_restart.arguments);
        // Fresh handles on the same backend root: what the next process
        // opens after the loss.
        let reopened_effect_host = backend
            .reopen()
            .await
            .expect("reopen durable backend after process loss")
            .effect_host();
        assert_eq!(
            lash::runtime::AwaitEventResolver::resolve_await_event(
                reopened_effect_host.as_ref(),
                &reopened_approvals
                    .completion_key(&after_restart.key)
                    .expect("read reopened completion key"),
                approvals::approval_resolution(&after_restart),
            )
            .await
            .expect("resolve through reopened effect host"),
            lash::ResolveOutcome::Accepted
        );
        reopened_approvals
            .mark_decided(&after_restart.key, approvals::ApprovalDecision::Approved)
            .expect("settle reopened approval row");
        let output = turn
            .await
            .expect("restart approval turn task")
            .expect("restart approval turn succeeds");
        assert_eq!(output.final_value(), Some(&json!("applied")));
    });
}

async fn async_completion_reopen_and_redrive(resolution: lash::Resolution, slug: &str) {
    let directory = tempfile::tempdir().expect("async completion directory");
    let approval_path = directory.path().join("approvals.db");
    let clock = Arc::new(lash::testing::TestClock::new(1_000_000));
    let approvals = approvals::WorkbenchApprovals::open(&approval_path).unwrap();
    let backend = approval_backend(directory.path(), Some(clock.clone())).await;
    let effect_host = backend.effect_host();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = lash::testing::TestProvider::builder()
        .kind("async-completion-redrive")
        .complete({
            let calls = calls.clone();
            move |_| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async {
                    Ok(text_response(
                        r#"<typescript>
try {
  const value = await ops.apply_change({ target: "async-demo", change: "reopen-redrive" });
  finish({ ok: true, value: value });
} catch (error) {
  finish({ ok: false, error: error.message, cause: error.cause });
}
</typescript>"#,
                    ))
                }
            }
        })
        .build()
        .into_handle();
    let core = approval_test_core(&backend, provider.clone(), approvals.clone()).await;
    let session_id = format!("async-completion-{slug}");
    let session = core.session(&session_id).open().await.unwrap();
    let scope = lash::durability::EffectHost::scoped_static(
        effect_host.as_ref(),
        lash::runtime::AdmittedScope::unpinned(session.turn_scope("async-turn"))
            .expect("a turn scope admits unpinned"),
    )
    .unwrap()
    .unwrap();
    let mut turn = tokio::spawn(async move {
        session
            .turn(lash::TurnInput::text("Apply async change"))
            .turn_id("async-turn")
            .require_finish()
            .unwrap()
            .advanced()
            .run_with_scope(scope)
            .await
    });
    let pending = wait_for_approval(&approvals, &mut turn).await;
    turn.abort();
    assert!(turn.await.unwrap_err().is_cancelled());
    drop(core);
    drop(effect_host);
    drop(backend);
    drop(approvals);
    // Worker loss leaves the effect claim leased. Expire it without waiting.
    clock.advance(60_000);
    let approvals = approvals::WorkbenchApprovals::open(&approval_path).unwrap();
    let backend = approval_backend(directory.path(), Some(clock.clone())).await;
    let effect_host = backend.effect_host();
    let core = approval_test_core(&backend, provider, approvals.clone()).await;
    let session = core.session(&session_id).open().await.unwrap();
    let key = approvals.completion_key(&pending.key).unwrap();
    assert_eq!(
        core.completions()
            .resolve(key.clone(), resolution.clone())
            .await
            .unwrap(),
        lash::ResolveOutcome::Accepted
    );
    let scope = lash::durability::EffectHost::scoped_static(
        effect_host.as_ref(),
        lash::runtime::AdmittedScope::unpinned(session.turn_scope("async-turn"))
            .expect("a turn scope admits unpinned"),
    )
    .unwrap()
    .unwrap();
    let output = session
        .turn(lash::TurnInput::text("Apply async change"))
        .turn_id("async-turn")
        .require_finish()
        .unwrap()
        .advanced()
        .run_with_scope(scope)
        .await
        .unwrap();
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "redrive must reuse the journaled provider response"
    );
    assert_eq!(
        core.completions()
            .resolve(key, resolution.clone())
            .await
            .unwrap(),
        lash::ResolveOutcome::AlreadyResolved {
            terminal: resolution.clone()
        },
        "redrive retains the exact typed terminal resolution"
    );
    let value = output.final_value().cloned();
    match resolution {
        lash::Resolution::Ok(expected) => {
            assert_eq!(
                value.as_ref(),
                Some(&json!({"ok": true, "value": expected}))
            )
        }
        lash::Resolution::Err(error) => {
            let value = value
                .as_ref()
                .expect("the program handles every tool outcome");
            assert_eq!(value["ok"], false);
            assert_eq!(value["error"], error.message);
            assert_eq!(value["cause"]["class"], "execution");
            assert_eq!(value["cause"]["code"], error.code.namespaced());
            assert_eq!(value["cause"]["source"], "tool");
            assert_eq!(value["cause"]["retry"]["type"], "never");
        }
        lash::Resolution::Timeout => {
            let value = value
                .as_ref()
                .expect("the program handles every tool outcome");
            assert_eq!(value["ok"], false);
            assert_eq!(value["error"], "pending tool completion timed out");
            assert_eq!(value["cause"]["class"], "timeout");
            assert_eq!(value["cause"]["code"], "tool_completion_timeout");
            assert_eq!(value["cause"]["source"], "runtime");
            assert_eq!(value["cause"]["retry"]["type"], "never");
        }
        lash::Resolution::Cancelled => {
            // ADR 0096 + the FIG-3271 cancellation contract: a cancelled call
            // is an uncatchable host terminal, not a rejection the guest's
            // catch can settle, so the turn ends cancelled without a value.
            assert!(
                matches!(
                    &output.result.outcome,
                    lash::TurnOutcome::Stopped(lash::TurnStop::Cancelled { .. })
                ),
                "the turn terminates cancelled instead of re-prompting the model: {:?}",
                output.result.outcome
            );
            assert_eq!(value, None, "a cancelled call produces no guest value");
        }
    }
    let history = session.read_view();
    let messages = serde_json::to_value(history.messages()).unwrap();
    let rendered = messages.to_string();
    assert_eq!(
        rendered.matches("Apply async change").count(),
        1,
        "one user input survives redrive"
    );
    let trajectories: Vec<_> = history
        .active_events()
        .iter()
        .filter_map(|record| match record {
            lash::persistence::SessionHistoryRecord::Protocol(event) => {
                event.payload.get("RlmTrajectoryEntry")
            }
            lash::persistence::SessionHistoryRecord::Conversation(_) => None,
        })
        .collect();
    assert_eq!(
        trajectories.len(),
        1,
        "redrive commits one trajectory entry"
    );
    let trajectory = trajectories[0];
    assert_eq!(trajectory["id"], "lashlang_step_async-turn_0");
    assert_eq!(
        trajectory["final_output"],
        serde_json::to_value(&value).unwrap(),
        "history retains the actual terminal result"
    );
    assert_eq!(trajectory["calls"].as_array().unwrap().len(), 1);
    assert_eq!(trajectory["calls"][0]["operation"], "ops.apply_change");
    let code = trajectory["code"].as_str().unwrap();
    assert!(
        code.contains("reopen-redrive"),
        "history retains the provider program"
    );
    assert!(
        code.contains("async-demo"),
        "history retains the tool arguments"
    );
    let before_reopen = serde_json::to_value(history.active_events()).unwrap();
    drop(session);
    drop(core);
    let reopened = approval_test_core(
        &backend,
        // The reopen must present the recorded provider pin: a different
        // provider id is refused as `ProviderMismatch` (ADR 0066). The
        // panicking completer still proves the read never reaches it.
        lash::testing::TestProvider::builder()
            .kind("async-completion-redrive")
            .complete(|_| async { panic!("reading history must not invoke the provider") })
            .build()
            .into_handle(),
        approvals,
    )
    .await;
    let session = reopened.session(&session_id).open().await.unwrap();
    assert_eq!(
        serde_json::to_value(session.read_view().active_events()).unwrap(),
        before_reopen,
        "terminal history survives another cold session reopen unchanged"
    );
}

#[test]
fn async_completion_success_crosses_session_reopen_and_redrive() {
    run_async_test_on_stack_budget("async-completion-success", || async {
        Box::pin(async_completion_reopen_and_redrive(
            lash::Resolution::Ok(json!({"status": "applied"})),
            "success",
        ))
        .await;
    });
}

#[test]
fn async_completion_failure_crosses_session_reopen_and_redrive() {
    run_async_test_on_stack_budget("async-completion-failure", || async {
        Box::pin(async_completion_reopen_and_redrive(
            approvals::denial_resolution(),
            "failure",
        ))
        .await;
    });
}

#[test]
fn async_completion_timeout_crosses_session_reopen_and_redrive() {
    run_async_test_on_stack_budget("async-completion-timeout", || async {
        Box::pin(async_completion_reopen_and_redrive(
            lash::Resolution::Timeout,
            "timeout",
        ))
        .await;
    });
}

#[test]
fn async_completion_cancel_crosses_session_reopen_and_redrive() {
    run_async_test_on_stack_budget("async-completion-cancel", || async {
        Box::pin(async_completion_reopen_and_redrive(
            lash::Resolution::Cancelled,
            "cancel",
        ))
        .await;
    });
}
