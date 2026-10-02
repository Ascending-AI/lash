use super::*;
use lash_core::ConfigCommandEntry;
use lash_core::plugin::ConfigRegistry;
use lash_core::testing::TestTurnExecution as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

fn registry() -> ConfigRegistry {
    ConfigRegistry::build(&[Arc::new(StandardProtocolPluginFactory::new())])
        .expect("standard config registry")
}

fn creation(
    prompt: Option<Value>,
    parent: Option<&lash_core::PluginConfig>,
) -> lash_core::PluginConfig {
    let mut options = lash_core::PluginOptions::default();
    if let Some(prompt) = prompt {
        options.insert_versioned(
            STANDARD_PROTOCOL_PLUGIN_ID,
            lash_core::FormatVersion::ONE,
            serde_json::json!({"prompt": prompt}),
        );
    }
    registry()
        .resolve_creation(
            Some(STANDARD_PROTOCOL_PLUGIN_ID),
            &options,
            parent,
            parent.is_none(),
            &lash_core::store::plugin_writers::PluginAdmission::default(),
        )
        .expect("standard prompt creation is accepted")
}

fn configured_prompt() -> Value {
    serde_json::json!({
        "intro": "You help maintain this project.",
        "instructions": ["Use the project's conventions."],
        "context": ["Working directory: /project"],
        "omit_builtin_guidance": false
    })
}

#[test]
fn creation_records_the_builtin_prompt_default() {
    let config = creation(None, None);
    assert_eq!(
        config
            .get(STANDARD_PROTOCOL_PLUGIN_ID)
            .expect("recorded namespace")["prompt"],
        serde_json::json!({"intro": null, "instructions": [], "context": [], "omit_builtin_guidance": false})
    );
    let recorded = config
        .decode::<StandardRecordedConfig>(STANDARD_PROTOCOL_PLUGIN_ID)
        .expect("decode config")
        .expect("standard config");
    assert_eq!(recorded.prompt, StandardPrompt::default());
    insta::assert_snapshot!(recorded.render_system_prompt(&lash_core::ToolCatalog::default()), @r"
    You are an assistant operating the lash harness.

    ## Execution

    Call tools directly with their declared JSON arguments. Use `batch` for two or more independent calls (at most 64 per batch); make dependent calls after their inputs return. Check each batch result’s success flag before using its value. Answer in prose only when no tool is needed.

    ## Guidance

    - Be concise; no filler, hedging, or performative tone.
    - Act as soon as the next step is clear; do not restate conclusions.
    - Prefer the simplest correct solution.
    ");
}

#[test]
fn creation_override_is_recorded_and_children_inherit_it() {
    let parent = creation(Some(configured_prompt()), None);
    assert_eq!(
        parent
            .get(STANDARD_PROTOCOL_PLUGIN_ID)
            .expect("parent namespace")["prompt"],
        configured_prompt()
    );
    let child = creation(None, Some(&parent));
    assert_eq!(
        child
            .get(STANDARD_PROTOCOL_PLUGIN_ID)
            .expect("child namespace")["prompt"],
        configured_prompt()
    );
    let explicit_child = creation(
        Some(serde_json::json!({"intro":"other defaults"})),
        Some(&parent),
    );
    assert_eq!(
        explicit_child
            .get(STANDARD_PROTOCOL_PLUGIN_ID)
            .expect("child namespace")["prompt"],
        configured_prompt()
    );
    let recorded = parent
        .decode::<StandardRecordedConfig>(STANDARD_PROTOCOL_PLUGIN_ID)
        .expect("decode parent")
        .expect("parent config");
    insta::assert_snapshot!(recorded.render_system_prompt(&module_catalog()), @r"
    You help maintain this project.

    ## Execution

    Call tools directly with their declared JSON arguments. Use `batch` for two or more independent calls (at most 64 per batch); make dependent calls after their inputs return. Check each batch result’s success flag before using its value. Answer in prose only when no tool is needed.

    ## Guidance

    - Be concise; no filler, hedging, or performative tone.
    - Act as soon as the next step is clear; do not restate conclusions.
    - Prefer the simplest correct solution.

    Use the project's conventions.

    ## Tool modules

    #### issues

    Search before reading an issue. Use cursors for pagination.

    #### deferred

    Deferred module instructions.

    ## Context

    Working directory: /project
    ");
}

#[tokio::test]
async fn prompt_commands_apply_only_to_the_next_run() {
    registry()
        .admit(
            "replace",
            0,
            vec![
                ConfigCommandEntry {
                    owner: STANDARD_PROTOCOL_PLUGIN_ID.into(),
                    command: "set_prompt".into(),
                    args: serde_json::json!({"prompt": configured_prompt()}),
                },
                ConfigCommandEntry {
                    owner: STANDARD_PROTOCOL_PLUGIN_ID.into(),
                    command: "set_prompt_context".into(),
                    args: serde_json::json!({"context": ["Updated working directory: /next"]}),
                },
            ],
        )
        .expect("both standard prompt commands are registered");
    let double = lash_restate_test::backend(
        super::tests::SEED,
        lash_restate_test::ServerConfig::default(),
    )
    .await
    .expect("Restate double");
    let backend = double.lash_backend();
    let session_id = lash_core::SessionId::from("standard-prompt-transaction");
    let mut config: lash_core::PersistedSessionConfig = prompt_policy().into();
    config.plugin_config = creation(None, None);
    let child = creation(None, Some(&config.plugin_config));
    let store = lash_core::runtime::admit_session_view(
        &backend.session_store_factory(),
        &lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            config,
            head: lash_core::SessionCreationHead::Config,
        },
    )
    .await
    .expect("create session");
    let observed = Arc::new(Mutex::new(Vec::new()));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = {
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        let calls = Arc::clone(&calls);
        lash_core::testing::TestProvider::builder()
            .kind("standard-prompt-law")
            .complete(move |_| {
                let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
                let entered = Arc::clone(&entered);
                let release = Arc::clone(&release);
                async move {
                    if first {
                        entered.notify_one();
                        release.notified().await;
                    }
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "done".into(),
                            response_meta: None,
                        }],
                        ..Default::default()
                    })
                }
            })
            .build()
            .into_handle()
    };
    let mut runtime = prompt_runtime(&backend, store.clone(), provider.clone(), &observed).await;
    let handler = double
        .open_handler(lash_core::AdmittedScope::turn(&session_id, "held-run"))
        .await
        .expect("held run handler");
    let submitting = async {
        entered.notified().await;
        let mut submitter =
            prompt_runtime(&backend, store.clone(), provider.clone(), &observed).await;
        let prompt: StandardPrompt = serde_json::from_value(configured_prompt()).expect("prompt");
        let receipt = submitter
            .submit_config_transaction(
                "replace",
                0,
                &lash_core::ConfigTransaction::of(SetStandardPrompt { prompt }).then(
                    SetStandardPromptContext {
                        context: vec!["Updated working directory: /next".into()],
                    },
                ),
            )
            .await
            .expect("submit prompt while run owns head");
        let head = store
            .load_session_head_meta()
            .await
            .expect("read held head")
            .expect("creation head");
        assert_eq!(head.config.config_revision, 0);
        assert_eq!(head.config.plugin_config, creation(None, None));
        assert!(
            store
                .list_queued_work()
                .await
                .expect("queued command")
                .iter()
                .any(|batch| batch.batch_id == receipt.batch_id)
        );
        release.notify_one();
        receipt
    };
    let (turn, receipt) = tokio::join!(
        runtime.execute_turn(
            lash_core::TurnInput::text("first"),
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                handler.scoped()
            )
        ),
        submitting
    );
    turn.expect("held run finishes");
    handler.close().await.expect("close held handler");
    assert_eq!(runtime.config_revision(), 0);
    assert_eq!(
        observed.lock().expect("observations").as_slice(),
        &[
            (0, StandardPrompt::default()),
            (0, StandardPrompt::default())
        ]
    );
    apply_prompt_command(&mut runtime, &double, &session_id, "replace", receipt).await;
    assert_eq!(runtime.config_revision(), 1);
    execute_prompt_run(&mut runtime, &double, &session_id, "next-run").await;
    let mut whole_prompt: StandardPrompt =
        serde_json::from_value(configured_prompt()).expect("prompt");
    whole_prompt.context = vec!["Updated working directory: /next".into()];
    assert_eq!(
        observed.lock().expect("observations")[2..],
        [(1, whole_prompt.clone()), (1, whole_prompt.clone())]
    );
    let replacement_context = Vec::new();
    let receipt = runtime
        .submit_config_transaction(
            "context",
            1,
            &lash_core::ConfigTransaction::of(SetStandardPromptContext {
                context: replacement_context.clone(),
            }),
        )
        .await
        .expect("submit context replacement");
    apply_prompt_command(&mut runtime, &double, &session_id, "context", receipt).await;
    execute_prompt_run(&mut runtime, &double, &session_id, "context-run").await;
    let mut context_prompt = whole_prompt;
    context_prompt.context = replacement_context;
    assert_eq!(
        observed.lock().expect("observations")[4..],
        [(2, context_prompt.clone()), (2, context_prompt)]
    );
    assert_eq!(
        child
            .decode::<StandardRecordedConfig>(STANDARD_PROTOCOL_PLUGIN_ID)
            .expect("child decode")
            .expect("child namespace")
            .prompt,
        StandardPrompt::default()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[test]
fn omitting_builtin_guidance_keeps_tool_modules_and_context() {
    let mut prompt = configured_prompt();
    prompt["omit_builtin_guidance"] = Value::Bool(true);
    let config = creation(Some(prompt), None);
    assert_eq!(
        config
            .get(STANDARD_PROTOCOL_PLUGIN_ID)
            .expect("recorded namespace")["prompt"]["omit_builtin_guidance"],
        true
    );
    let mut recorded = config
        .decode::<StandardRecordedConfig>(STANDARD_PROTOCOL_PLUGIN_ID)
        .expect("decode config")
        .expect("standard config");
    let text = recorded.render_system_prompt(&module_catalog());
    assert!(!text.contains("## Execution"));
    assert!(!text.contains("Be concise"));
    assert!(text.contains("Use the project's conventions."));
    assert_eq!(text.matches("Search before reading an issue.").count(), 1);
    assert!(text.ends_with("Working directory: /project"));
    recorded.prompt.intro = Some("".into());
    assert!(
        !recorded
            .render_system_prompt(&module_catalog())
            .contains("You are an assistant")
    );
    recorded.behaviour.discovery_operation = Some("issue_search".into());
    let text = recorded.render_system_prompt(&module_catalog());
    assert_eq!(text.matches("Search before reading an issue.").count(), 1);
    assert!(!text.contains("Deferred module instructions."));
    assert!(text.ends_with("Working directory: /project"));
}

fn module_catalog() -> lash_core::ToolCatalog {
    let tools = ["issue_search", "issue_read", "hidden"].map(|name| {
        let mut tool = lash_core::ToolDefinition::raw(
            name,
            name,
            "Issue operation",
            serde_json::json!({"type":"object"}),
            serde_json::json!({"type":"string"}),
        )
        .expect("valid declared tool schemas");
        let hidden = name == "hidden";
        tool.manifest.inline = !hidden;
        tool.manifest.module = Some(Arc::new(lash_core::ToolModule {
            name: if hidden { "deferred" } else { "issues" }.into(),
            instructions: Some(
                if hidden {
                    "Deferred module instructions."
                } else {
                    "Search before reading an issue. Use cursors for pagination."
                }
                .into(),
            ),
        }));
        tool
    });
    let catalog = lash_core::ToolCatalog::from_tool_definitions(Vec::from(tools));
    serde_json::from_value(serde_json::to_value(catalog).expect("record catalog"))
        .expect("read recorded catalog")
}

fn prompt_policy() -> lash_core::SessionPolicy {
    lash_core::SessionPolicy {
        model: Some(lash_core::testing::test_llm_profile_config(
            "standard-prompt-law",
            lash_core::testing::test_llm_profile_metadata("standard-prompt-law"),
        )),
        ..lash_core::SessionPolicy::new(
            lash_core::TurnBudget::bounded(4),
            lash_core::MaxToolCalls::new(1024),
        )
    }
}

type ObservedPrompts = Arc<Mutex<Vec<(u64, StandardPrompt)>>>;

async fn prompt_runtime(
    backend: &lash_core::Backend,
    store: lash_core::store::SessionStore,
    provider: lash_core::facade_support::ProviderHandle,
    observed: &ObservedPrompts,
) -> lash_core::facade_support::LashRuntime {
    let observe: lash_core::plugin::BeforeTurnHook = {
        let observed = Arc::clone(observed);
        Arc::new(move |ctx| {
            let observed = Arc::clone(&observed);
            Box::pin(async move {
                let recorded = ctx
                    .plugin_config
                    .decode::<StandardRecordedConfig>(STANDARD_PROTOCOL_PLUGIN_ID)
                    .expect("admitted namespace")
                    .expect("recorded namespace");
                observed
                    .lock()
                    .expect("observations")
                    .push((ctx.plugin_config.revision, recorded.prompt));
                Ok(Vec::new())
            })
        })
    };
    let after: lash_core::plugin::AfterTurnHook = {
        let observed = Arc::clone(observed);
        Arc::new(move |ctx| {
            let observed = Arc::clone(&observed);
            Box::pin(async move {
                let recorded = ctx
                    .plugin_config
                    .decode::<StandardRecordedConfig>(STANDARD_PROTOCOL_PLUGIN_ID)
                    .expect("admitted namespace")
                    .expect("recorded namespace");
                observed
                    .lock()
                    .expect("observations")
                    .push((ctx.plugin_config.revision, recorded.prompt));
                Ok(Vec::new())
            })
        })
    };
    let mut host = lash_core::facade_support::RuntimeHostConfig::new(
        backend.clone(),
        lash_core::CommitBudget::bounded(8 * 1024 * 1024, 1024),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.models = lash_core::testing::llm_profiles_serving(&prompt_policy(), provider);
    Box::pin(
        lash_core::facade_support::LashRuntime::builder(
            host,
            lash_core::testing::runtime_lease_owner(),
        )
        .with_store(store)
        .with_policy(prompt_policy())
        .with_plugin_factories(vec![
            Arc::new(StandardProtocolPluginFactory::new()),
            Arc::new(lash_core::plugin::StaticPluginFactory::new(
                lash_core::plugin::PluginDeclaration::initial("observe-standard-prompt"),
                lash_core::facade_support::PluginSpec::new()
                    .with_before_turn(observe)
                    .with_after_turn(after),
            )),
        ])
        .build(),
    )
    .await
    .expect("prompt runtime")
}

async fn execute_prompt_run(
    runtime: &mut lash_core::facade_support::LashRuntime,
    double: &lash_restate_test::RestateTestBackend,
    session_id: &lash_core::SessionId,
    run: &str,
) {
    let handler = double
        .open_handler(lash_core::AdmittedScope::turn(
            session_id,
            lash_core::TurnId::fixture(run.to_string()),
        ))
        .await
        .expect("run handler");
    runtime
        .execute_turn(
            lash_core::TurnInput::text(run),
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                handler.scoped(),
            ),
        )
        .await
        .expect("run finishes");
    handler.close().await.expect("close run handler");
}

async fn apply_prompt_command(
    runtime: &mut lash_core::facade_support::LashRuntime,
    double: &lash_restate_test::RestateTestBackend,
    session_id: &lash_core::SessionId,
    command: &str,
    receipt: lash_core::facade_support::SessionCommandReceipt,
) {
    let handler = double
        .open_handler(lash_core::AdmittedScope::turn(
            session_id,
            lash_core::TurnId::fixture(command.to_string()),
        ))
        .await
        .expect("command handler");
    runtime
        .execute_next_run(
            command,
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                handler.scoped(),
            ),
        )
        .await
        .expect("apply prompt command");
    handler.close().await.expect("close command handler");
    assert!(matches!(
        runtime
            .settle_session_command(receipt)
            .await
            .expect("settlement"),
        lash_core::runtime::SessionCommandSettlement::Applied {
            outcome: lash_core::runtime::SessionCommandOutcome::ConfigTransaction {
                outcome: lash_core::ConfigTransactionOutcome::Applied { .. }
            },
            ..
        }
    ));
}
