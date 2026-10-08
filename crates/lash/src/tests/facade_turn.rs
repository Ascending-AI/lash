//! L3 (FIG-5172): a sent input's turn runs on the core's node, through the
//! production turn driver, and its handle answers the committed reply.

use super::*;

/// FIG-5264: the run handle exposes a queued withdrawal directly, so a host
/// can distinguish work that never ran from an interrupted run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_queued_input_answers_withdrawn_and_it_never_runs() {
    let accounts = AccountCore::new().await;
    let session = accounts
        .core
        .session(crate::SessionId::from("facade-queued-cancel"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let held = session
        .send(crate::TurnInput::text("hold"))
        .await
        .expect("accepted");
    accounts
        .entered
        .acquire()
        .await
        .expect("model entered")
        .forget();
    let run = crate::TurnId::from("facade-withdrawn");
    let queued = session
        .send(crate::TurnInput::text("withdraw me"))
        .id(run.clone())
        .await
        .expect("queued");
    let receipt = session
        .cancel(crate::CancelTarget::Run(run))
        .await
        .expect("cancel");
    assert!(
        matches!(receipt, crate::CancelReceipt::Withdrawn { input: Some(ref input), .. } if input == queued.input_id()),
        "{receipt:?}"
    );
    let second = session
        .send(crate::TurnInput::text("withdraw by cold input id"))
        .await
        .expect("queued second input");
    let receipt = session
        .attach(second.input_id().clone())
        .cancel()
        .await
        .expect("cold input cancel");
    assert!(
        matches!(receipt, crate::CancelReceipt::Withdrawn { input: Some(ref input), .. } if input == second.input_id()),
        "{receipt:?}"
    );
    assert!(matches!(
        queued.outcome().await.expect("withdrawn outcome"),
        crate::SendOutcome::Withdrawn { .. }
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
    let session_id = lash_sansio::SessionId::try_from("facade-turn".to_owned()).expect("id");
    let session = core
        .session(session_id)
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let output = session
        .send(crate::TurnInput::text("hello"))
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
    let provider = crate::testing::TestProvider::builder()
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
    .build(crate::testing::runtime_lease_owner())
    .expect("standard core");
    let session = core
        .session(lash_sansio::SessionId::try_from("attributed-turn".to_owned()).expect("id"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let turn = crate::TurnId::parse("host-attributed-turn").expect("turn id");
    let output = session
        .send(crate::TurnInput::text("hello"))
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
        Some(crate::provider::LlmTurnScope {
            run: crate::RunId::from(turn.clone()),
            turn_id: turn,
        })
    );
    assert_eq!(scope.attempt, Some(1));
    core.shutdown().await.expect("shutdown");
}

/// The task the frame switch hands the new frame.
const TASK: &str = "carry on from the summary";
/// A tool that switches the agent frame and hands the new frame [`TASK`].
struct SwitchFrame;

#[async_trait::async_trait]
impl crate::tools::StaticToolExecute for SwitchFrame {
    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true }))
            .with_control(lash_core::ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material("facade-frame-switch")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some(TASK.to_owned()),
            })
            .into()
    }
}

/// FIG-5232: compaction's continuation through `send()`. A turn whose tool
/// switches the agent frame with a task answers its send with the switch,
/// and the session runs the task as its next turn, on the new frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frame_switch_through_send_completes_and_its_follow_on_runs_next() {
    let definition = lash_core::ToolDefinition::raw(
        "switch_frame",
        "switch_frame",
        "Switches the agent frame and hands the new frame a task.",
        serde_json::json!({ "type": "object", "additionalProperties": false, "properties": {} }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("switch_frame's schemas");
    // The model compacts by calling `switch_frame`, and answers the task it
    // finds on the new frame.
    let provider = crate::testing::TestProvider::builder()
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
                    tool_name: "switch_frame".to_owned(),
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
    .tools(Arc::new(crate::tools::StaticToolProvider::new(
        vec![definition],
        SwitchFrame,
    )))
    .build(crate::testing::runtime_lease_owner())
    .expect("standard core");
    let session_id =
        lash_sansio::SessionId::try_from("facade-frame-switch".to_owned()).expect("id");
    let session = core
        .session(session_id)
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");

    let switched = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        session
            .send(crate::TurnInput::text("compact, then carry on"))
            .output(),
    )
    .await
    .expect("the switching send completes")
    .expect("the switching turn answers");
    assert_eq!(
        switched.status(),
        crate::TurnStatus::Answered,
        "{switched:?}"
    );
    let lash_core::facade_support::TurnOutcome::AgentFrameSwitch {
        frame_key, task, ..
    } = &switched.result.outcome
    else {
        panic!("the switch answers its send: {:?}", switched.result.outcome);
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

/// The tool an account add brings: the workbench's mail tools, in small.
const ACCOUNT_TOOL: &str = "inbox_work";

/// A tool source whose tools are the accounts added so far.
struct AccountTools {
    names: Arc<StdMutex<Vec<String>>>,
}

fn account_tool(name: &str) -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "Reads an account's inbox.",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "string" }),
    )
    .expect("the account tool's schemas")
}

#[async_trait]
impl ToolProvider for AccountTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        self.names
            .lock_recover()
            .iter()
            .map(|name| account_tool(name).manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        self.names
            .lock_recover()
            .iter()
            .any(|tool| tool == name)
            .then(|| Arc::new(account_tool(name).contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!("empty")).into()
    }
}

/// The tools each model request offered, by the request's input text.
type OfferedTools = Arc<StdMutex<Vec<(String, Vec<String>)>>>;

/// A core over SQLite memory stores whose model echoes each input and
/// records the tools each request offered, by the input's text. A turn
/// whose input is `hold` waits, once it reached the model, for `release`.
struct AccountCore {
    core: LashCore,
    accounts: Arc<StdMutex<Vec<String>>>,
    offered: OfferedTools,
    entered: Arc<tokio::sync::Semaphore>,
    release: Arc<tokio::sync::Semaphore>,
}

impl AccountCore {
    async fn new() -> Self {
        let accounts = Arc::new(StdMutex::new(Vec::new()));
        let offered: OfferedTools = Arc::new(StdMutex::new(Vec::new()));
        let entered = Arc::new(tokio::sync::Semaphore::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let provider = crate::testing::TestProvider::builder()
            .kind("account-tools")
            .complete({
                let offered = Arc::clone(&offered);
                let entered = Arc::clone(&entered);
                let release = Arc::clone(&release);
                move |request: LlmRequest| {
                    let text = last_user_text(&request);
                    let tools = request.tools.iter().map(|tool| tool.name.clone()).collect();
                    offered.lock_recover().push((text.clone(), tools));
                    let (entered, release) = (Arc::clone(&entered), Arc::clone(&release));
                    async move {
                        if text == "hold" {
                            entered.add_permits(1);
                            release.acquire().await.expect("released").forget();
                        }
                        Ok(text_response(&format!("echo: {text}")))
                    }
                }
            })
            .build()
            .into_handle();
        let core = explicit_ephemeral_facets(LashCore::standard_builder(
            sqlite_memory_store_backend().await,
        ))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .tools(Arc::new(AccountTools {
            names: Arc::clone(&accounts),
        }))
        .build(crate::testing::runtime_lease_owner())
        .expect("standard core");
        Self {
            core,
            accounts,
            offered,
            entered,
            release,
        }
    }

    /// The tools the request for input `text` offered.
    fn offered_to(&self, text: &str) -> Vec<String> {
        self.offered
            .lock_recover()
            .iter()
            .find(|(input, _)| input == text)
            .map(|(_, tools)| tools.clone())
            .unwrap_or_else(|| panic!("no request for {text:?}"))
    }
}

/// Add an account as the workbench does: the account's tool appears at its
/// source, and a tool-catalog refresh is queued on the session.
async fn add_account(
    accounts: &AccountCore,
    admin: &crate::LashSession,
    key: &str,
) -> lash_core::facade_support::SessionCommandReceipt {
    accounts
        .accounts
        .lock_recover()
        .push(ACCOUNT_TOOL.to_owned());
    admin
        .admin()
        .commands()
        .refresh_tool_catalog("account_added", key)
        .await
        .expect("the refresh is queued")
}

fn assert_settled(settlement: &lash_core::runtime::SessionCommandSettlement) {
    assert!(
        matches!(
            settlement,
            lash_core::runtime::SessionCommandSettlement::Durable(_)
        ),
        "the refresh settles: {settlement:?}"
    );
}

/// FIG-5245: a tool-catalog refresh queued while the session is idle after
/// it ran a turn (the workbench's account add) is applied by the session
/// actor, so the workbench's next `/api/turn` (its model selection, a config
/// transaction, then the send) runs, and that turn offers the new tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refresh_queued_while_the_session_is_idle_settles_and_the_next_turn_sees_it() {
    let accounts = AccountCore::new().await;
    let session_id = lash_sansio::SessionId::try_from("idle-refresh".to_owned()).expect("id");
    let session = accounts
        .core
        .session(session_id.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let first = session
        .send(crate::TurnInput::text("first"))
        .output()
        .await
        .expect("the first turn answers");
    assert!(first.is_success(), "{first:?}");
    assert!(
        !accounts
            .offered_to("first")
            .contains(&ACCOUNT_TOOL.to_owned())
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
            crate::config::ConfigWrite::new("model-selection", revision),
            crate::config::ConfigTransaction::of(crate::config::SetLlmProfile {
                model: crate::LlmProfileKey::new(mock_llm_profile_spec().wire_model),
            }),
        )
        .await
        .expect("the model selection settles");
    assert!(
        matches!(
            selected,
            crate::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "{selected:?}"
    );
    let next = session
        .send(crate::TurnInput::text("second"))
        .output()
        .await
        .expect("the next turn answers");
    assert_eq!(next.assistant_message(), Some("echo: second"), "{next:?}");
    assert!(
        accounts
            .offered_to("second")
            .contains(&ACCOUNT_TOOL.to_owned()),
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
    let session_id = lash_sansio::SessionId::try_from("in-turn-refresh".to_owned()).expect("id");
    let session = accounts
        .core
        .session(session_id.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let admin = accounts
        .core
        .session(session_id)
        .open()
        .await
        .expect("opened");
    let commands = admin.admin().commands();

    let held = session.send(crate::TurnInput::text("hold")).output();
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
            .contains(&ACCOUNT_TOOL.to_owned())
    );

    let settled = commands.settle(receipt).await.expect("the refresh settles");
    assert_settled(&settled);
    let next = session
        .send(crate::TurnInput::text("after"))
        .output()
        .await
        .expect("the next turn answers");
    assert_eq!(next.assistant_message(), Some("echo: after"), "{next:?}");
    assert!(
        accounts
            .offered_to("after")
            .contains(&ACCOUNT_TOOL.to_owned()),
        "the next turn offers the account's tool"
    );
    accounts.core.shutdown().await.expect("shutdown");
}

/// FIG-5263: every call of all three steps survives an owner loss between
/// steps, and a cold follower reads the same records without live activity.
#[tokio::test]
async fn three_step_tool_calls_survive_owner_loss_and_cold_reattachment() {
    let (_, capture) = lash_core::testing::trace_capture::capturing(|| async {
        let mut uninterrupted = None;
        for crash in [false, true] {
            let stores = sqlite_memory_store_set().await;
            let entered = Arc::new(tokio::sync::Notify::new());
            let calls = Arc::new(AtomicUsize::new(0));
            let provider = crate::testing::TestProvider::builder()
                .kind("three-steps")
                .complete({
                    let entered = Arc::clone(&entered);
                    let calls = Arc::clone(&calls);
                    move |request: LlmRequest| {
                        let entered = Arc::clone(&entered);
                        let calls = Arc::clone(&calls);
                        async move {
                            let results = request
                                .messages
                                .iter()
                                .flat_map(|message| message.blocks.iter())
                                .filter(|block| matches!(block, LlmContentBlock::ToolResult { .. }))
                                .count();
                            if crash && results == 2 && calls.fetch_add(1, Ordering::SeqCst) == 0 {
                                entered.notify_one();
                                std::future::pending::<()>().await;
                            }
                            if results == 6 {
                                return Ok(text_response("all six calls finished"));
                            }
                            Ok(LlmResponse {
                                parts: (results..results + 2)
                                    .map(|index| LlmOutputPart::ToolCall {
                                        call_id: format!("call-{index}"),
                                        tool_name: lash_core::testing::FIXTURE_ECHO_TOOL.to_owned(),
                                        input_json:
                                            serde_json::json!({"value": format!("value-{index}")})
                                                .to_string(),
                                        replay: None,
                                    })
                                    .collect(),
                                ..LlmResponse::default()
                            })
                        }
                    }
                })
                .build()
                .into_handle();
            let build = |owner: &str| {
                explicit_ephemeral_facets(LashCore::standard_builder(
                    lash_conformance::backend_over(stores.clone()),
                ))
                .serve_test_llm_profile(provider.clone(), mock_llm_profile_spec())
                .tools(Arc::new(lash_core::testing::FixtureTools))
                .build(lash_core::LeaseOwnerIdentity::opaque(owner, "boot"))
                .expect("core")
            };
            let old = build("three-steps-old");
            let id = lash_sansio::SessionId::try_from("three-steps".to_owned()).expect("id");
            let session = old
                .session(id.clone())
                .create(crate::SessionCreation::root(mock_session_spec()))
                .await
                .expect("created");
            let input = crate::TurnId::parse("three-step-input").expect("input id");
            let handle = session
                .send(crate::TurnInput::text("call six tools"))
                .id(input.clone());
            let output = tokio::spawn(handle.output());
            let new = if crash {
                tokio::time::timeout(std::time::Duration::from_secs(60), entered.notified())
                    .await
                    .expect("second step started");
                // Stop drops the active turn in the second model call. The next
                // node restores the checkpoint after the first round committed.
                old.node.stop().await;
                Some(build("three-steps-new"))
            } else {
                None
            };
            let output = tokio::time::timeout(std::time::Duration::from_secs(60), output)
                .await
                .expect("turn finishes")
                .expect("output task")
                .expect("turn answers");
            assert_eq!(output.assistant_message(), Some("all six calls finished"));
            let records = &output.result.tool_calls;
            assert_eq!(records.len(), 6, "all calls, crash={crash}: {records:?}");
            for (index, record) in records.iter().enumerate() {
                assert_eq!(
                    record.provider_call_id.as_deref(),
                    Some(format!("call-{index}").as_str())
                );
                assert_eq!(
                    record.args,
                    serde_json::json!({"value": format!("value-{index}")})
                );
                assert!(record.output.is_success());
            }
            if let Some(expected) = &uninterrupted {
                assert_eq!(
                    records, expected,
                    "a resumed turn reports the uninterrupted records"
                );
            } else {
                uninterrupted = Some(records.clone());
            }
            let core = new.as_ref().unwrap_or(&old);
            let durable = core.session(id).durable().await.expect("durable session");
            let cold = durable
                .attach_id(input)
                .output()
                .await
                .expect("cold answer");
            assert_eq!(
                cold.result.tool_calls, *records,
                "no live activity is needed"
            );
            if let Some(new) = new {
                new.shutdown().await.expect("shutdown new");
            }
            old.shutdown().await.expect("shutdown old");
        }
    })
    .await;
    let warnings: Vec<_> = capture
        .events
        .lock_recover()
        .iter()
        .filter(|event| {
            event.level == "WARN"
                && event.target == "lash_core::runtime::observation"
                && event.contains_field("message")
                && event
                    .field("message")
                    .contains("failed to capture plugin query services")
        })
        .cloned()
        .collect();
    assert!(
        warnings.is_empty(),
        "normal durable turns warned: {warnings:?}"
    );
}

/// FIG-5264: an accepted running cancel retains its request evidence and the
/// run eventually records the cancellation in its terminal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_running_input_answers_cancelled_with_accepted_detail() {
    let accounts = AccountCore::new().await;
    let session = accounts
        .core
        .session(crate::SessionId::from("facade-running-cancel"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let held = session
        .send(crate::TurnInput::text("hold"))
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
    let crate::CancelReceipt::Cancelled { run, receipt } = cancelled else {
        panic!("running cancellation: {cancelled:?}");
    };
    assert_eq!(held.run().await.expect("bound run"), Some(run));
    let crate::TurnCancelOutcome::Requested(evidence) = receipt.outcome else {
        panic!("accepted request: {receipt:?}");
    };
    assert_eq!(evidence.request_id, "facade-stop");
    assert_eq!(evidence.origin.as_deref(), Some("operator"));
    assert_eq!(evidence.reason.as_deref(), Some("stop"));
    assert_eq!(
        held.output().await.expect("terminal").status(),
        crate::TurnStatus::Cancelled
    );
    accounts.core.shutdown().await.expect("shutdown");
}

/// FIG-5264: absent and completed ids accept no cancellation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_an_unknown_or_completed_run_answers_unknown_or_revoked() {
    let core = standard_core_over(sqlite_memory_store_backend().await);
    let session = core
        .session(crate::SessionId::from("facade-unknown-cancel"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    assert!(matches!(
        session
            .run(crate::TurnId::from("absent").into())
            .cancel()
            .await
            .expect("unknown"),
        crate::CancelReceipt::UnknownOrRevoked
    ));
    let answered = session
        .send(crate::TurnInput::text("answer"))
        .await
        .expect("accepted");
    let cancel = answered.cancel();
    answered.output().await.expect("answered");
    assert!(matches!(
        cancel.await.expect("completed"),
        crate::CancelReceipt::UnknownOrRevoked
    ));
    core.shutdown().await.expect("shutdown");
}

/// FIG-5264: an operation's atomic batch withdrawal uses the same receipt,
/// without inventing input identity or cancellation evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_queued_operation_answers_withdrawn_and_repeats_are_unknown() {
    let accounts = AccountCore::new().await;
    let session_id = crate::SessionId::from("facade-operation-withdraw");
    let session = accounts
        .core
        .session(session_id.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let held = session
        .send(crate::TurnInput::text("hold"))
        .await
        .expect("accepted");
    accounts
        .entered
        .acquire()
        .await
        .expect("model entered")
        .forget();
    let parts = session.send_parts().await.expect("session parts");
    let batch = parts
        .store
        .enqueue_queued_work(
            crate::persistence::QueuedWorkBatchDraft::new(
                session_id.clone(),
                lash_core::DeliveryPolicy::AfterCurrentTurnCommit,
                crate::persistence::QueuedWorkPayload::SessionCommand {
                    command: Box::new(lash_core::facade_support::SessionCommand::RunPluginTask {
                        name: "never invoked".to_owned(),
                        args: serde_json::json!({}),
                    }),
                },
            )
            .with_source_key("command:facade-operation-withdraw"),
        )
        .await
        .expect("queued task");
    let operation = lash_core::tool_run::OperationRun {
        session_id,
        operation_id: batch.batch_id.to_string(),
    };
    let handle = session.run(operation.run_id().into());
    let receipt = handle.cancel().await.expect("withdraw task");
    assert!(
        matches!(receipt, crate::CancelReceipt::Withdrawn { ref run, input: None } if *run == operation.run_id()),
        "{receipt:?}"
    );
    assert!(
        !session
            .queued_work()
            .await
            .expect("queue")
            .iter()
            .any(|open| open.batch_id == batch.batch_id)
    );
    assert!(matches!(
        handle.cancel().await.expect("repeat"),
        crate::CancelReceipt::UnknownOrRevoked
    ));
    let missing = lash_core::tool_run::OperationRun {
        session_id: session.session_id().clone(),
        operation_id: "missing-operation".to_owned(),
    };
    assert!(matches!(
        session
            .run(missing.run_id().into())
            .cancel()
            .await
            .expect("missing task"),
        crate::CancelReceipt::UnknownOrRevoked
    ));
    accounts.release.add_permits(1);
    held.output().await.expect("held turn answers");
    accounts.core.shutdown().await.expect("shutdown");
}

/// FIG-5264: a terminal operation cannot be withdrawn or acquire a new cancel.
#[tokio::test]
async fn cancelling_a_settled_operation_answers_unknown_or_revoked() {
    use lash_core::durable_port::domain::{DomainWrite, SessionMailWrite, TurnWrite};
    use lash_core::durable_port::{ActorKey, CommitLabel, NodeId, NodeSpec};
    let backend = sqlite_memory_store_backend().await;
    let core = standard_core_over(backend.clone());
    let session_id = crate::SessionId::from("facade-operation-settled");
    let session = core
        .session(session_id.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    // Seed terminal evidence without a live owner competing with the fixture.
    core.shutdown().await.expect("shutdown");
    let parts = session.send_parts().await.expect("session parts");
    let batch = parts
        .store
        .enqueue_queued_work(
            crate::persistence::QueuedWorkBatchDraft::new(
                session_id.clone(),
                lash_core::DeliveryPolicy::AfterCurrentTurnCommit,
                crate::persistence::QueuedWorkPayload::SessionCommand {
                    command: Box::new(lash_core::facade_support::SessionCommand::RunPluginTask {
                        name: "settled task".to_owned(),
                        args: serde_json::json!({}),
                    }),
                },
            )
            .with_source_key("command:facade-operation-settled"),
        )
        .await
        .expect("queued task");
    let operation = lash_core::tool_run::OperationRun {
        session_id: session_id.clone(),
        operation_id: batch.batch_id.to_string(),
    };
    let database = backend.durable();
    let actor = ActorKey::session(session_id.as_str()).expect("actor");
    let snapshot = database
        .actor(&actor)
        .await
        .expect("actor read")
        .expect("woken actor");
    let lease = database
        .register_node(&NodeSpec {
            node: NodeId::new("settled-operation-fixture"),
            decodes: vec![snapshot.formats],
            ttl_millis: 15_000,
        })
        .await
        .expect("fixture lease");
    let claimed = database.claim(&lease, 1).await.expect("claim");
    assert_eq!(claimed.len(), 1);
    let mut tx = database
        .begin(&actor, claimed[0].epoch)
        .await
        .expect("transaction");
    tx.write(DomainWrite::SessionMail(SessionMailWrite::Admit {
        session: session_id.clone(),
        run: operation.run_id(),
        inputs: Vec::new(),
        batches: vec![batch.batch_id.clone()],
    }));
    tx.write(DomainWrite::Turn(TurnWrite::Admit {
        session: session_id.clone(),
        run: operation.run_id(),
        admission: lash_core::store::RunAdmissionRecord::Operation {
            batch: batch.batch_id,
        },
        turn_deadline: None,
    }));
    database
        .commit(tx, CommitLabel::TURN_ADMIT)
        .await
        .expect("admitted");
    let mut tx = database
        .begin(&actor, claimed[0].epoch)
        .await
        .expect("terminal transaction");
    tx.write(DomainWrite::Turn(TurnWrite::Terminal {
        session: session_id,
        run: operation.run_id(),
        cause: Box::new(lash_core::store::RunTerminalCause::Refused {
            code: lash_core::RuntimeErrorCode::SessionCommandRun,
            message: "operation refused before execution".to_owned(),
            refusal_cause: None,
        }),
        head_revision: None,
    }));
    database
        .commit(tx, CommitLabel::TURN_COMMIT)
        .await
        .expect("terminal");
    assert!(matches!(
        session
            .run(operation.run_id().into())
            .cancel()
            .await
            .expect("settled task cancel"),
        crate::CancelReceipt::UnknownOrRevoked
    ));
}

/// A host's prompt plugin: it replaces the standard protocol's intro with a
/// wrapper and adds a late section of its own (ADR 0133).
struct HostSections;

impl PluginFactory for HostSections {
    fn id(&self) -> &'static str {
        "host_sections"
    }

    fn build(
        &self,
        _ctx: &lash_core::plugin::PluginSessionContext,
    ) -> std::result::Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError>
    {
        Ok(Arc::new(HostSections))
    }
}

impl lash_core::plugin::PluginDefinition for HostSections {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("host_sections")
    }
}

impl lash_core::plugin::SessionPlugin for HostSections {
    fn id(&self) -> &'static str {
        "host_sections"
    }

    fn register(
        &self,
        reg: &mut lash_core::plugin::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        use crate::plugins::{
            PromptInput, PromptSectionSpec, PromptWrapSpec, PromptWrapTarget, SectionText,
        };
        use crate::prompt::{PromptPlacement, PromptSectionId, PromptSectionKey, PromptWrapKey};
        reg.prompt().wrap(
            PromptWrapSpec::new(
                PromptWrapKey::new("intro").expect("valid wrap key"),
                PromptSectionId::new(
                    crate::standard::STANDARD_PROTOCOL_PLUGIN_ID,
                    PromptSectionKey::new(crate::standard::standard_section_keys::INTRO)
                        .expect("valid section key"),
                ),
            ),
            Arc::new(
                |_: &PromptInput<'_>, _: PromptWrapTarget<'_>, _: SectionText| {
                    Ok(SectionText::text("You are the release desk."))
                },
            ),
        )?;
        reg.prompt().section(
            PromptSectionSpec::new(
                PromptSectionKey::new("release").expect("valid section key"),
                PromptPlacement::CurrentContext,
            ),
            Arc::new(|_: &PromptInput<'_>| Ok(SectionText::text("Release 4.2 freezes on Friday."))),
        )
    }
}

/// FIG-5257: a turn's model call composes its sections at its admission
/// (FIG-5255) and places them on the model request: the standard
/// protocol's in the instructions, over the offered tools, with a host
/// wrapper's replacement intro, and a host's late section after the
/// projected conversation, outside it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turns_request_carries_its_sections_where_they_are_placed() {
    let sent: Arc<StdMutex<Vec<LlmRequest>>> = Arc::new(StdMutex::new(Vec::new()));
    let provider = crate::testing::TestProvider::builder()
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
    .tools(Arc::new(AccountTools {
        names: Arc::new(StdMutex::new(vec!["ask".to_owned()])),
    }))
    .plugin(Arc::new(HostSections))
    .build(crate::testing::runtime_lease_owner())
    .expect("standard core");
    let session_id = lash_sansio::SessionId::try_from("prompt-sections".to_owned()).expect("id");
    let session = core
        .session(session_id)
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let output = session
        .send(crate::TurnInput::text("hello"))
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
        instructions.starts_with("You are the release desk.\n\n## Execution\n\n"),
        "{instructions}"
    );
    assert!(
        instructions.contains("Ask only when progress is blocked."),
        "the offered `ask` tool reaches the guidance: {instructions}"
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

/// A host plugin whose one section refuses to render.
struct BrokenSection;

impl PluginFactory for BrokenSection {
    fn id(&self) -> &'static str {
        "broken_section"
    }

    fn build(
        &self,
        _ctx: &lash_core::plugin::PluginSessionContext,
    ) -> std::result::Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError>
    {
        Ok(Arc::new(BrokenSection))
    }
}

impl lash_core::plugin::PluginDefinition for BrokenSection {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("broken_section")
    }
}

impl lash_core::plugin::SessionPlugin for BrokenSection {
    fn id(&self) -> &'static str {
        "broken_section"
    }

    fn register(
        &self,
        reg: &mut lash_core::plugin::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        use crate::plugins::{PromptInput, PromptRenderError, PromptSectionSpec, SectionText};
        use crate::prompt::{PromptPlacement, PromptSectionKey};
        reg.prompt().section(
            PromptSectionSpec::new(
                PromptSectionKey::new("calendar").expect("valid section key"),
                PromptPlacement::CurrentContext,
            ),
            Arc::new(|_: &PromptInput<'_>| {
                Err::<SectionText, _>(PromptRenderError::new(
                    "the release calendar is unreachable",
                ))
            }),
        )
    }
}

/// FIG-5255 (ADR 0133 §6, §7): a call whose section refuses to render fails
/// closed at its admission: no earlier text stands in, and nothing reaches
/// the provider.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_whose_section_refuses_fails_closed_and_sends_nothing() {
    let sent: Arc<StdMutex<Vec<LlmRequest>>> = Arc::new(StdMutex::new(Vec::new()));
    let provider = crate::testing::TestProvider::builder()
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
    .build(crate::testing::runtime_lease_owner())
    .expect("standard core");
    let session_id = lash_sansio::SessionId::try_from("broken-section".to_owned()).expect("id");
    let session = core
        .session(session_id)
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let output = session
        .send(crate::TurnInput::text("hello"))
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
