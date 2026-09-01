async fn approval_test_core(
    data_dir: &std::path::Path,
    provider: ProviderHandle,
    approvals: approvals::WorkbenchApprovals,
    effect_host: Arc<dyn lash::durability::EffectHost>,
) -> LashCore {
    let artifact_store = Arc::new(
        lash_sqlite_store::Store::open(&data_dir.join("artifacts.db"))
            .await
            .expect("open approval test artifact store"),
    ) as Arc<dyn lash::persistence::LashlangArtifactStore>;
    let process_env_store = Arc::new(
        lash_sqlite_store::Store::open(&data_dir.join("process-env.db"))
            .await
            .expect("open approval test process env store"),
    ) as Arc<dyn lash::persistence::ProcessExecutionEnvStore>;
    let trigger_store = Arc::new(
        lash_sqlite_store::SqliteTriggerStore::open(&data_dir.join("triggers.db"))
            .await
            .expect("open approval test trigger store"),
    );
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .wall_clock(lash::rlm::WallClockBound::secs(30))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build()
        .with_lashlang_abilities(workbench_lashlang_abilities()),
        artifact_store,
    );
    let runtime_host_config = lash::durability::RuntimeHostConfig::new(
        effect_host,
        Arc::new(lash::persistence::FileAttachmentStore::new(
            data_dir.join("attachments"),
        )),
        process_env_store,
        lash::CommitBudget::bounded(1024 * 1024, 512),
        lash::QueuedWorkBatchingConfig::new(1),
    );
    LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .provider(provider)
        .session_spec(
            lash::SessionSpec::new()
                .turn_budget(lash::TurnBudget::Unbounded),
        )
        .model(test_model())
        .store_factory(Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
        ))
        .plugin(Arc::new(
            WorkbenchPluginFactory::new("").with_approvals(approvals),
        ))
        .trigger_store(trigger_store)
        .without_queued_work()
        .advanced()
        .runtime_host_config(runtime_host_config)
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
        let effect_host = Arc::new(
            lash_sqlite_store::SqliteEffectHost::open(&directory.path().join("effects.db"))
                .await
                .expect("open durable effect host"),
        );
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-approval-approve")
            .complete(|_| async {
                Ok(text_response(
                    r#"<lashlang>
result = await ops.apply_change({ target: "demo-cluster", change: "enable safe mode" })?
finish result
</lashlang>"#,
                ))
            })
            .build()
            .into_handle();
        let core = approval_test_core(
            directory.path(),
            provider,
            approvals.clone(),
            effect_host.clone(),
        )
        .await;
        let session = core
            .session("approval-approve")
            .open()
            .await
            .expect("open approval session");
        let turn_scope = lash::durability::EffectHost::scoped_static(
            effect_host.as_ref(),
            lash::runtime::ExecutionScope::turn("approval-approve", "approval-approve-turn"),
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
        let accepted = core
            .completions()
            .resolve(key.clone(), approvals::approval_resolution(&approval))
            .await
            .expect("approve completion");
        assert_eq!(accepted, lash::ResolveOutcome::Accepted);
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
            .mark_decided(&approval.key, "approved")
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

#[test]
fn approval_denial_is_typed_until_the_current_lashlang_bridge_stringifies_it() {
    run_async_test_on_stack_budget("workbench-approval-deny", || async {
        let directory = tempfile::tempdir().expect("approval tempdir");
        let approvals = approvals::WorkbenchApprovals::open(directory.path().join("approvals.db"))
            .expect("open approval ledger");
        let effect_host = Arc::new(
            lash_sqlite_store::SqliteEffectHost::open(&directory.path().join("effects.db"))
                .await
                .expect("open durable effect host"),
        );
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-approval-deny")
            .complete(|_| async {
                Ok(text_response(
                    r#"<lashlang>
result = await ops.apply_change({ target: "demo-cluster", change: "disable audit log" })
finish result
</lashlang>"#,
                ))
            })
            .build()
            .into_handle();
        let core = approval_test_core(
            directory.path(),
            provider,
            approvals.clone(),
            effect_host.clone(),
        )
        .await;
        let session = core
            .session("approval-deny")
            .open()
            .await
            .expect("open denial session");
        let turn_scope = lash::durability::EffectHost::scoped_static(
            effect_host.as_ref(),
            lash::runtime::ExecutionScope::turn("approval-deny", "approval-deny-turn"),
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
            .mark_decided(&approval.key, "denied")
            .expect("settle denial row");
        let output = turn
            .await
            .expect("denial turn task")
            .expect("denial is handled in Lashlang");
        let final_value = output.final_value().expect("denial wrapper");
        assert_eq!(final_value.get("ok"), Some(&Value::Bool(false)));
        let serialized_failure = final_value
            .get("error")
            .and_then(Value::as_str)
            .expect("current Lashlang bridge stringifies host errors");
        let typed_failure: Value =
            serde_json::from_str(serialized_failure).expect("serialized typed tool failure");
        assert_eq!(typed_failure.get("class"), Some(&json!("execution")));
        assert_eq!(typed_failure.get("code"), Some(&json!("approval_denied")));
        assert_eq!(
            typed_failure.get("message"),
            Some(&json!("the operator denied this change"))
        );
        assert_eq!(typed_failure.get("source"), Some(&json!("tool")));
    });
}

#[test]
fn approval_restart_reopens_the_ledger_and_durable_effect_host() {
    run_async_test_on_stack_budget("workbench-approval-restart", || async {
        let directory = tempfile::tempdir().expect("approval tempdir");
        let approval_path = directory.path().join("approvals.db");
        let effect_path = directory.path().join("effects.db");
        let approvals = approvals::WorkbenchApprovals::open(&approval_path)
            .expect("open approval ledger");
        let effect_host = Arc::new(
            lash_sqlite_store::SqliteEffectHost::open(&effect_path)
                .await
                .expect("open durable effect host"),
        );
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-approval-restart")
            .complete(|_| async {
                Ok(text_response(
                    r#"<lashlang>
result = await ops.apply_change({ target: "restart-demo", change: "rotate workers" })?
finish result.status
</lashlang>"#,
                ))
            })
            .build()
            .into_handle();
        let core = approval_test_core(
            directory.path(),
            provider,
            approvals.clone(),
            effect_host.clone(),
        )
        .await;
        let session = core
            .session("approval-restart")
            .open()
            .await
            .expect("open restart session");
        let turn_scope = lash::durability::EffectHost::scoped_static(
            effect_host.as_ref(),
            lash::runtime::ExecutionScope::turn("approval-restart", "approval-restart-turn"),
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
        let reopened_effect_host = lash_sqlite_store::SqliteEffectHost::open(&effect_path)
            .await
            .expect("reopen durable effect host after process loss");
        assert_eq!(
            lash::runtime::AwaitEventResolver::resolve_await_event(
                &reopened_effect_host,
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
            .mark_decided(&after_restart.key, "approved")
            .expect("settle reopened approval row");
        let output = turn
            .await
            .expect("restart approval turn task")
            .expect("restart approval turn succeeds");
        assert_eq!(output.final_value(), Some(&json!("applied")));
    });
}
