//! L3 (FIG-5172): a sent input's turn runs on the core's node, through the
//! production turn driver, and its handle answers the committed reply.

use super::*;

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
