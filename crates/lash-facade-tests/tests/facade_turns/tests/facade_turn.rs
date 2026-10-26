use super::*;

use lash_facade_tests::turn::*;

/// FIG-5264: the run handle exposes a queued withdrawal directly, so a host
/// can distinguish work that never ran from an interrupted run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_queued_input_answers_withdrawn_and_it_never_runs() {
    let accounts = AccountCore::new().await;
    let session = accounts
        .core
        .session(lash_kernel_value::identity::SessionId::from(
            "facade-queued-cancel",
        ))
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let held = session
        .send(lash::TurnInput::text("hold"))
        .await
        .expect("accepted");
    accounts
        .entered
        .acquire()
        .await
        .expect("model entered")
        .forget();
    let run = lash_vocabulary::identity::TurnId::from("facade-withdrawn");
    let queued = session
        .send(lash::TurnInput::text("withdraw me"))
        .id(run.clone())
        .await
        .expect("queued");
    let receipt = session
        .cancel(lash::CancelTarget::Run(
            lash_vocabulary::identity::RunId::parse((run).as_str()).unwrap(),
        ))
        .await
        .expect("cancel");
    assert!(
        matches!(receipt, lash::CancelReceipt::Withdrawn { input: Some(ref input), .. } if input == queued.input_id()),
        "{receipt:?}"
    );
    let second = session
        .send(lash::TurnInput::text("withdraw by cold input id"))
        .await
        .expect("queued second input");
    let receipt = session
        .attach(second.input_id().clone())
        .cancel()
        .await
        .expect("cold input cancel");
    assert!(
        matches!(receipt, lash::CancelReceipt::Withdrawn { input: Some(ref input), .. } if input == second.input_id()),
        "{receipt:?}"
    );
    assert!(matches!(
        queued.outcome().await.expect("withdrawn outcome"),
        lash::SendOutcome::Withdrawn { .. }
    ));
    accounts.release.add_permits(1);
    held.output().await.expect("held turn answers");
    assert_eq!(
        accounts.offered.lock_recover().len(),
        1,
        "only the held input ran"
    );
    accounts.core.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sent_input_runs_on_the_cores_node_and_answers_its_reply() {
    let core = standard_core_over(sqlite_memory_store_backend().await);
    let session_id =
        lash_kernel_value::identity::SessionId::try_from("facade-turn".to_owned()).expect("id");
    let session = core
        .session(session_id)
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let output = session
        .send(lash::TurnInput::text("hello"))
        .output()
        .await
        .expect("the turn answers");
    assert!(output.is_success(), "{output:?}");
    assert_eq!(output.assistant_message(), Some("echo: hello"));
    core.shutdown().await.expect("shutdown");
}

/// FIG-5219: a host attributes a model call to its turn, Run and attempt from
/// the typed scope its provider receives, never by parsing the request id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turns_model_call_carries_its_typed_turn_run_and_attempt() {
    let scopes = Arc::new(StdMutex::new(Vec::new()));
    let provider = lash::testing::TestProvider::builder()
        .kind("embed-test")
        .complete({
            let scopes = Arc::clone(&scopes);
            move |request| {
                scopes.lock_recover().push(request.scope.clone());
                async move { Ok(text_response("attributed")) }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .build(lash_facade_tests::runtime_lease_owner())
    .expect("standard core");
    let session = core
        .session(
            lash_kernel_value::identity::SessionId::try_from("attributed-turn".to_owned())
                .expect("id"),
        )
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let turn = lash_vocabulary::identity::TurnId::parse("host-attributed-turn").expect("turn id");
    let output = session
        .send(lash::TurnInput::text("hello"))
        .id(turn.clone())
        .output()
        .await
        .expect("the turn answers");
    assert!(output.is_success(), "{output:?}");

    let scopes = scopes.lock_recover().clone();
    let [scope] = scopes.as_slice() else {
        panic!("one model call: {scopes:?}");
    };
    assert_eq!(
        scope.turn,
        Some(lash::provider::LlmTurnScope {
            run: lash_vocabulary::identity::RunId::parse(turn.as_str()).unwrap(),
            turn_id: turn,
        })
    );
    assert_eq!(scope.attempt, Some(1));
    core.shutdown().await.expect("shutdown");
}

/// FIG-5232: compaction's continuation through `send()`. A turn whose tool
/// switches the agent frame with a task answers its send with the switch,
/// and the session runs the task as its next turn, on the new frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frame_switch_through_send_completes_and_its_follow_on_runs_next() {
    let definition = lash_vocabulary::tool_contract::ToolDefinition::control(
        "switch_frame",
        "tools.switch_frame",
        "Switches the agent frame and hands the new frame a task.",
        serde_json::json!({ "type": "object", "additionalProperties": false, "properties": {} }),
        lash_kernel_value::tool_declaration::TurnControls::switch_agent_frame(),
    )
    .expect("switch_frame's schemas")
    .with_execution(std::time::Duration::from_secs(120));
    // The model compacts by calling `switch_frame`, and answers the task it
    // finds on the new frame.
    let provider = lash::testing::TestProvider::builder()
        .kind("facade-frame-switch")
        .requires_streaming(true)
        .complete(|request: LlmRequest| async move {
            let user_text = last_user_text(&request);
            if user_text.contains(TASK) {
                return Ok(text_response(&format!("done: {user_text}")));
            }
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "facade-frame-switch-call".to_owned(),
                    tool_name: "tools__switch_frame".to_owned(),
                    input_json: "{}".to_owned(),
                    replay: None,
                }],
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .tools(
        lash::tool_provider_key!("tools"),
        Arc::new(lash::tools::StaticToolProvider::new(
            vec![definition],
            SwitchFrame,
        )),
    )
    .build(lash_facade_tests::runtime_lease_owner())
    .expect("standard core");
    let session_id =
        lash_kernel_value::identity::SessionId::try_from("facade-frame-switch".to_owned())
            .expect("id");
    let session = core
        .session(session_id)
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");

    let switched = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        session
            .send(lash::TurnInput::text("compact, then carry on"))
            .output(),
    )
    .await
    .expect("the switching send completes")
    .expect("the switching turn answers");
    assert_eq!(
        switched.status(),
        lash::TurnStatus::Answered,
        "{switched:?}"
    );
    let lash_core::facade_support::TurnOutcome::AgentFrameSwitch {
        frame_key, task, ..
    } = &switched.outcome
    else {
        panic!("the switch answers its send: {:?}", switched.outcome);
    };
    assert_eq!(task, TASK);

    // The switch's commit mailed the task: the session's next turn runs it
    // on the new frame, and answers under its own run.
    let follow_on = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        session
            .attach_id(lash_core::runtime::durable::session_mail::frame_task_run(
                frame_key,
            ))
            .output(),
    )
    .await
    .expect("the follow-on completes")
    .expect("the follow-on answers");
    assert_eq!(
        follow_on.assistant_message(),
        Some(format!("done: {TASK}").as_str()),
        "{follow_on:?}"
    );
    core.shutdown().await.expect("shutdown");
}

/// FIG-5245: a tool-catalog refresh queued while the session is idle after
/// it ran a turn (the workbench's account add) is applied by the session
/// actor, so the workbench's next `/api/turn` (its model selection, a config
/// transaction, then the send) runs, and that turn offers the new tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refresh_queued_while_the_session_is_idle_settles_and_the_next_turn_sees_it() {
    let accounts = AccountCore::new().await;
    let session_id =
        lash_kernel_value::identity::SessionId::try_from("idle-refresh".to_owned()).expect("id");
    let session = accounts
        .core
        .session(session_id.clone())
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let first = session
        .send(lash::TurnInput::text("first"))
        .output()
        .await
        .expect("the first turn answers");
    assert!(first.is_success(), "{first:?}");
    assert!(
        !accounts
            .offered_to("first")
            .contains(&format!("tools__{ACCOUNT_TOOL}"))
    );

    let admin = accounts
        .core
        .session(session_id)
        .open()
        .await
        .expect("opened");
    let receipt = add_account(&accounts, &admin, "idle-refresh-1").await;
    let settled = admin
        .admin()
        .commands()
        .settle(receipt)
        .await
        .expect("the refresh settles");
    assert_settled(&settled);

    // The workbench's `/api/turn`: the model selection, then the send.
    let config = admin.admin().config();
    let revision = config.revision().await.expect("revision");
    let selected = config
        .apply(
            lash::config::ConfigWrite::new("model-selection", revision),
            lash::config::ConfigTransaction::of(lash::config::SetLlmProfile {
                model: lash::LlmProfileKey::new(mock_llm_profile_spec().wire_model),
            }),
        )
        .await
        .expect("config accepted")
        .await_outcome(&config)
        .await
        .expect("the model selection settles");
    assert!(
        matches!(
            selected,
            lash::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "{selected:?}"
    );
    let next = session
        .send(lash::TurnInput::text("second"))
        .output()
        .await
        .expect("the next turn answers");
    assert_eq!(next.assistant_message(), Some("echo: second"), "{next:?}");
    assert!(
        accounts
            .offered_to("second")
            .contains(&format!("tools__{ACCOUNT_TOOL}")),
        "the next turn offers the account's tool"
    );
    accounts.core.shutdown().await.expect("shutdown");
}

/// FIG-5245: a tool-catalog refresh queued while a turn runs waits for that
/// turn: it is applied after the turn commits, and the next turn offers the
/// new tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refresh_queued_during_a_turn_applies_after_its_commit_and_the_next_turn_sees_it() {
    let accounts = AccountCore::new().await;
    let session_id =
        lash_kernel_value::identity::SessionId::try_from("in-turn-refresh".to_owned()).expect("id");
    let session = accounts
        .core
        .session(session_id.clone())
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let admin = accounts
        .core
        .session(session_id)
        .open()
        .await
        .expect("opened");
    let commands = admin.admin().commands();

    let held = session.send(lash::TurnInput::text("hold")).output();
    let queued = async {
        accounts
            .entered
            .acquire()
            .await
            .expect("the held turn reached its model")
            .forget();
        let receipt = add_account(&accounts, &admin, "in-turn-refresh-1").await;
        // The running turn holds the session: the refresh waits for it.
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(500),
                commands.settle(receipt.clone()),
            )
            .await
            .is_err(),
            "the refresh waits for the running turn"
        );
        accounts.release.add_permits(1);
        receipt
    };
    let (held, receipt) = tokio::join!(held, queued);
    let held = held.expect("the held turn answers");
    assert_eq!(held.assistant_message(), Some("echo: hold"), "{held:?}");
    assert!(
        !accounts
            .offered_to("hold")
            .contains(&format!("tools__{ACCOUNT_TOOL}"))
    );

    let settled = commands.settle(receipt).await.expect("the refresh settles");
    assert_settled(&settled);
    let next = session
        .send(lash::TurnInput::text("after"))
        .output()
        .await
        .expect("the next turn answers");
    assert_eq!(next.assistant_message(), Some("echo: after"), "{next:?}");
    assert!(
        accounts
            .offered_to("after")
            .contains(&format!("tools__{ACCOUNT_TOOL}")),
        "the next turn offers the account's tool"
    );
    accounts.core.shutdown().await.expect("shutdown");
}

/// FIG-5264: an accepted running cancel retains its request evidence and the
/// run eventually records the cancellation in its terminal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_running_input_answers_cancelled_with_accepted_detail() {
    let accounts = AccountCore::new().await;
    let session = accounts
        .core
        .session(lash_kernel_value::identity::SessionId::from(
            "facade-running-cancel",
        ))
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let held = session
        .send(lash::TurnInput::text("hold"))
        .await
        .expect("accepted");
    accounts
        .entered
        .acquire()
        .await
        .expect("model entered")
        .forget();
    let request = held
        .cancel()
        .request_id("facade-stop")
        .origin("operator")
        .reason("stop");
    let cancelled = request.await.expect("cancel");
    let lash::CancelReceipt::Cancelled { run, receipt } = cancelled else {
        panic!("running cancellation: {cancelled:?}");
    };
    assert_eq!(held.run().await.expect("bound run"), Some(run));
    let lash::TurnCancelOutcome::Requested(evidence) = receipt.outcome else {
        panic!("accepted request: {receipt:?}");
    };
    assert_eq!(evidence.request_id, "facade-stop");
    assert_eq!(evidence.origin.as_deref(), Some("operator"));
    assert_eq!(evidence.reason.as_deref(), Some("stop"));
    assert_eq!(
        held.output().await.expect("terminal").status(),
        lash::TurnStatus::Cancelled
    );
    accounts.core.shutdown().await.expect("shutdown");
}

/// FIG-5264: absent and completed ids accept no cancellation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_an_unknown_or_completed_run_answers_unknown_or_revoked() {
    let core = standard_core_over(sqlite_memory_store_backend().await);
    let session = core
        .session(lash_kernel_value::identity::SessionId::from(
            "facade-unknown-cancel",
        ))
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    assert!(matches!(
        session
            .run(lash_vocabulary::identity::RunId::parse("absent").unwrap())
            .cancel()
            .await
            .expect("unknown"),
        lash::CancelReceipt::UnknownOrRevoked
    ));
    let answered = session
        .send(lash::TurnInput::text("answer"))
        .await
        .expect("accepted");
    let cancel = answered.cancel();
    answered.output().await.expect("answered");
    assert!(matches!(
        cancel.await.expect("completed"),
        lash::CancelReceipt::UnknownOrRevoked
    ));
    core.shutdown().await.expect("shutdown");
}

/// FIG-5257: a turn's model call composes its sections at its admission
/// (FIG-5255) and places them on the model request: the standard
/// protocol's in the instructions, over the offered tools, with a host
/// section's identity, and a host's late section after the
/// projected conversation, outside it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turns_request_carries_its_sections_where_they_are_placed() {
    let sent: Arc<StdMutex<Vec<LlmRequest>>> = Arc::new(StdMutex::new(Vec::new()));
    let provider = lash::testing::TestProvider::builder()
        .kind("prompt-sections")
        .complete({
            let sent = Arc::clone(&sent);
            move |request: LlmRequest| {
                sent.lock_recover().push(request.clone());
                async move { Ok(text_response("noted")) }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .tools(
        lash::tool_provider_key!("tools"),
        Arc::new(AccountTools {
            names: Arc::new(StdMutex::new(vec!["ask".to_owned()])),
        }),
    )
    .plugin(Arc::new(HostSections))
    .build(lash_facade_tests::runtime_lease_owner())
    .expect("standard core");
    let session_id =
        lash_kernel_value::identity::SessionId::try_from("prompt-sections".to_owned()).expect("id");
    let session = core
        .session(session_id)
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let output = session
        .send(lash::TurnInput::text("hello"))
        .output()
        .await
        .expect("the turn answers");
    assert!(output.is_success(), "{output:?}");

    let request = sent
        .lock_recover()
        .first()
        .cloned()
        .expect("one model call");
    let instructions = request.instructions.as_deref().expect("instructions");
    assert!(
        instructions.starts_with("## Execution\n\n")
            && instructions.ends_with("You are the release desk."),
        "{instructions}"
    );
    assert!(
        !instructions.contains("Ask only when progress is blocked."),
        "interaction guidance belongs to the host: {instructions}"
    );
    assert!(!instructions.contains("Release 4.2"), "{instructions}");
    let late = request.messages.last().expect("the late context");
    assert_eq!(late.role, LlmRole::User);
    assert!(matches!(
        late.blocks.as_slice(),
        [LlmContentBlock::Text { text, .. }] if text.as_ref() == "Release 4.2 freezes on Friday."
    ));
    assert_eq!(
        request.messages.len(),
        2,
        "one user input and one late context"
    );
    assert!(matches!(request.messages[0].blocks.as_slice(),
        [LlmContentBlock::Text { text, .. }] if text.as_ref() == "hello"));
    assert!(!late.starts_user_segment);
    core.shutdown().await.expect("shutdown");
}

/// FIG-5255 (ADR 0133 §6, §7): a call whose section refuses to render fails
/// closed at its admission: no earlier text stands in, and nothing reaches
/// the provider.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_whose_section_refuses_fails_closed_and_sends_nothing() {
    let sent: Arc<StdMutex<Vec<LlmRequest>>> = Arc::new(StdMutex::new(Vec::new()));
    let provider = lash::testing::TestProvider::builder()
        .kind("prompt-sections")
        .complete({
            let sent = Arc::clone(&sent);
            move |request: LlmRequest| {
                sent.lock_recover().push(request.clone());
                async move { Ok(text_response("noted")) }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .plugin(Arc::new(BrokenSection))
    .build(lash_facade_tests::runtime_lease_owner())
    .expect("standard core");
    let session_id =
        lash_kernel_value::identity::SessionId::try_from("broken-section".to_owned()).expect("id");
    let session = core
        .session(session_id)
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let output = session
        .send(lash::TurnInput::text("hello"))
        .output()
        .await
        .expect("the turn ends");
    assert!(!output.is_success(), "{output:?}");
    assert!(
        sent.lock_recover().is_empty(),
        "no request reached the model"
    );

    core.shutdown().await.expect("shutdown");
}
