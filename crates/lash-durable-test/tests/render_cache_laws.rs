//! A session's stored presentations keep the model's history cache prefix
//! across a render-options command, a renderer change, a run's own render
//! options and a core restart: what a turn rendered is committed once and
//! read back, never rendered again (ported by FIG-5310 from the deleted
//! `render_cache_law.rs` of lash-protocol-standard and lash-protocol-rlm).
//!
//! Each law runs on the SQLite tiers, whose store sets retain the full value
//! a cut output stores; the served PostgreSQL leg has no attachment store.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash_core::llm::types::{LlmContentBlock, LlmMessage, LlmRequest, LlmResponse};
use lash_sansio::sync::MutexExt as _;

use served::{Tier, WATCHDOG};

/// The model: it answers request `n` with `script(n)` and keeps every
/// request it served, across every core of a law.
struct Model {
    calls: AtomicUsize,
    requests: Mutex<Vec<LlmRequest>>,
    script: fn(usize, &LlmRequest) -> LlmResponse,
}

impl Model {
    fn new(script: fn(usize, &LlmRequest) -> LlmResponse) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            requests: Mutex::default(),
            script,
        })
    }

    fn provider(self: &Arc<Self>) -> lash_core::facade_support::ProviderHandle {
        let model = Arc::clone(self);
        lash_core::testing::TestProvider::builder()
            .kind("render-cache-model")
            .requires_streaming(true)
            .complete(move |request: LlmRequest| {
                let model = Arc::clone(&model);
                async move {
                    let call = model.calls.fetch_add(1, Ordering::SeqCst);
                    let response = (model.script)(call, &request);
                    model.requests.lock_recover().push(request);
                    Ok(response)
                }
            })
            .build()
            .into_handle()
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn request(&self, index: usize) -> LlmRequest {
        self.requests
            .lock_recover()
            .get(index)
            .cloned()
            .unwrap_or_else(|| panic!("the model was asked request {index}"))
    }
}

/// The index of the request's last cache breakpoint, if it marks one.
fn breakpoint(request: &LlmRequest) -> Option<usize> {
    request.messages.iter().rposition(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                LlmContentBlock::Text {
                    cache_breakpoint: true,
                    ..
                }
            )
        })
    })
}

/// The messages up to the request's last cache breakpoint: the history the
/// provider caches. The RLM protocol marks it.
fn history_prefix(request: &LlmRequest) -> Vec<LlmMessage> {
    let last = breakpoint(request).expect("the request marks its history breakpoint");
    request.messages[..=last].to_vec()
}

/// A standard request's history: up to its breakpoint, or the whole
/// transcript when it marks none.
fn standard_history(request: &LlmRequest) -> Vec<LlmMessage> {
    let last = breakpoint(request).unwrap_or_else(|| {
        request
            .messages
            .len()
            .checked_sub(1)
            .expect("history messages")
    });
    request.messages[..=last].to_vec()
}

/// `messages` encoded with their breakpoints cleared: what must stay
/// byte-identical for the cache to hit.
fn stable_bytes(messages: &[LlmMessage]) -> Vec<u8> {
    let mut messages = messages.to_vec();
    for message in &mut messages {
        for block in Arc::make_mut(&mut message.blocks).iter_mut() {
            if let LlmContentBlock::Text {
                cache_breakpoint, ..
            } = block
            {
                *cache_breakpoint = false;
            }
        }
    }
    serde_json::to_vec(&messages).expect("history JSON")
}

/// The full values a law's cut outputs stored.
async fn retained(backend: &lash::Backend) -> usize {
    backend
        .attachment_store()
        .list()
        .await
        .expect("list the stored attachments")
        .len()
}

fn owner(build: &'static str) -> lash::persistence::LeaseOwnerIdentity {
    lash::persistence::LeaseOwnerIdentity::opaque("render-cache-deployment", build)
}

fn session_id(name: &str) -> lash::SessionId {
    lash::SessionId::try_from(name.to_owned()).expect("a session id")
}

/// Send `input` on `session`'s core, with `options` as its run's protocol
/// options when given, and wait for its settled output.
async fn send(
    core: &lash::LashCore,
    session: &str,
    input: &str,
    options: Option<lash::runtime::ProtocolTurnOptions>,
) -> lash::TurnOutput {
    let session = core
        .session(session_id(session))
        .open()
        .await
        .expect("the session opens");
    let mut send = session.send(lash::TurnInput::text(input));
    if let Some(options) = options {
        send = send.protocol_turn_options(options);
    }
    tokio::time::timeout(WATCHDOG, send.output())
        .await
        .unwrap_or_else(|_| panic!("deadlock watchdog: the turn `{input}` never settled"))
        .expect("the turn answers")
}

/// Apply `command` to `session`'s config through its host admin.
async fn apply<C: lash_core::ConfigCommand>(core: &lash::LashCore, session: &str, command: C) {
    let session = core
        .session(session_id(session))
        .open()
        .await
        .expect("the session opens");
    let config = session.admin().config();
    let revision = config.revision().await.expect("read the config revision");
    let outcome = config
        .apply(
            lash::config::ConfigWrite::new("render-options-command", revision),
            lash::config::ConfigTransaction::of(command),
        )
        .await
        .expect("the render command is accepted")
        .await_outcome(&config)
        .await
        .expect("the render command settles");
    assert!(
        matches!(
            outcome,
            lash::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "the render command applies: {outcome:?}"
    );
}

// ---- the standard protocol ----

/// A tool output renderer whose identity and counts switch with `mode`.
#[derive(Default)]
struct SwitchingRenderer {
    mode: AtomicUsize,
    first: AtomicUsize,
    second: AtomicUsize,
    first_limit: AtomicUsize,
    second_limit: AtomicUsize,
}

impl lash::render::ToolOutputRenderer for SwitchingRenderer {
    fn id(&self) -> &str {
        if self.mode.load(Ordering::SeqCst) == 0 {
            "law.first"
        } else {
            "law.second"
        }
    }

    fn tool_output(
        &self,
        output: &lash_core::ToolCallOutput,
        tool: &lash_core::ToolId,
        params: &lash::render::ToolRenderParams,
    ) -> lash::render::Rendered<Vec<lash_core::facade_support::ModelToolReturnPart>> {
        if self.mode.load(Ordering::SeqCst) == 0 {
            self.first.fetch_add(1, Ordering::SeqCst);
            self.first_limit
                .store(params.value.max_chars, Ordering::SeqCst);
        } else {
            self.second.fetch_add(1, Ordering::SeqCst);
            self.second_limit
                .store(params.value.max_chars, Ordering::SeqCst);
        }
        lash::render::BuiltinToolOutputRenderer.tool_output(output, tool, params)
    }
}

fn fixture_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "law:fixture",
        "fixture",
        "Return a long result",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

/// The tool: a long result, then a short one, then long ones, each of its
/// own content so that every cut result stores a value of its own.
#[derive(Default)]
struct FixtureTool {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for FixtureTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![fixture_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "fixture").then(|| Arc::new(fixture_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let value = match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => "a".repeat(800),
            1 => "short uncut".to_owned(),
            2 => "b".repeat(800),
            _ => "c".repeat(800),
        };
        lash_core::ToolOutcome::ok(serde_json::json!(value)).into()
    }
}

/// Two fixture calls, then prose; one call and prose twice; then prose.
fn standard_script(call: usize, request: &LlmRequest) -> LlmResponse {
    match call {
        0 => served::response(
            (0..2)
                .map(|index| {
                    served::call(
                        &format!("fixture-0-{index}"),
                        "fixture",
                        serde_json::json!({}),
                    )
                })
                .collect(),
        ),
        2 | 4 => served::response(vec![served::call(
            &format!("fixture-{call}"),
            "fixture",
            serde_json::json!({}),
        )]),
        _ => served::text(request, "done"),
    }
}

fn standard_render(max_chars: usize) -> lash::render::StandardRenderConfig {
    lash::render::StandardRenderConfig {
        defaults: lash::render::ToolRenderPatch {
            value: lash::render::RenderParamsPatch {
                max_chars: Some(max_chars),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

fn standard_core(
    backend: &lash::Backend,
    model: &Arc<Model>,
    renderer: &Arc<SwitchingRenderer>,
    tool: &Arc<FixtureTool>,
    build: &'static str,
) -> lash::LashCore {
    lash::LashCore::builder(backend.clone())
        .protocol_plugin(Arc::new(
            lash::plugins::StandardProtocolPluginFactory::with_config(
                lash::plugins::StandardProtocolConfig {
                    renderer: lash::render::ToolOutputRendererSlot(
                        Arc::clone(renderer) as Arc<dyn lash::render::ToolOutputRenderer>
                    ),
                    ..lash::plugins::StandardProtocolConfig::default()
                },
            ),
        ))
        .tools(Arc::clone(tool) as Arc<dyn lash_core::ToolProvider>)
        .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(model.provider(), served::metadata())
        .build(owner(build))
        .expect("the core builds")
}

/// A standard session's rendered tool results stay in its history as they
/// were presented: a render-options command, a renderer change, a run's own
/// render options and a restarted core each change only what is rendered
/// afterwards, the history's cache prefix stays byte-identical, and nothing
/// stored is rendered or retained again.
async fn standard_runtime_keeps_recorded_history_across_params_renderer_and_reopen(tier: Tier) {
    const SESSION: &str = "standard-render-cache";
    let Some((stores, keep)) = served::stores(tier).await else {
        return;
    };
    let backend = served::backend(stores);
    let model = Model::new(standard_script);
    let renderer = Arc::new(SwitchingRenderer::default());
    let tool = Arc::new(FixtureTool::default());

    let core = standard_core(&backend, &model, &renderer, &tool, "first-build");
    core.session(session_id(SESSION))
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            served::spec(64)
                .plugin(
                    lash::standard::STANDARD_PROTOCOL_PLUGIN_ID,
                    lash::standard::StandardTurnOptions {
                        render: Some(standard_render(140)),
                    },
                )
                .expect("the render options encode"),
        ))
        .await
        .expect("the session is created");
    send(&core, SESSION, "first", None).await;
    assert_eq!(model.calls(), 2);
    assert_eq!(renderer.first.load(Ordering::SeqCst), 2);
    assert_eq!(renderer.first_limit.load(Ordering::SeqCst), 140);
    assert_eq!(retained(&backend).await, 1, "the cut result's full value");
    let prefix_len = standard_history(&model.request(1)).len();
    let prefix = stable_bytes(&standard_history(&model.request(1)));
    let spelled = String::from_utf8_lossy(&prefix).into_owned();
    assert!(spelled.contains("[output cut:"), "{spelled}");
    assert!(spelled.contains("short uncut"), "{spelled}");

    apply(
        &core,
        SESSION,
        lash::standard::SetStandardRender {
            render: Some(standard_render(120)),
        },
    )
    .await;
    renderer.mode.store(1, Ordering::SeqCst);
    send(&core, SESSION, "second", None).await;
    assert_eq!(renderer.first.load(Ordering::SeqCst), 2);
    assert_eq!(renderer.second.load(Ordering::SeqCst), 1);
    assert_eq!(renderer.second_limit.load(Ordering::SeqCst), 120);
    assert_eq!(retained(&backend).await, 2);
    assert_eq!(
        stable_bytes(&model.request(2).messages[..prefix_len]),
        prefix,
        "a render command and a new renderer keep the cached history"
    );
    assert!(format!("{:?}", model.request(3)).contains("[output cut:"));

    send(
        &core,
        SESSION,
        "run-spec",
        Some(
            lash::runtime::ProtocolTurnOptions::typed(lash::standard::StandardRunOptions {
                render: Some(standard_render(100)),
            })
            .expect("the run's render options encode"),
        ),
    )
    .await;
    assert_eq!(renderer.second.load(Ordering::SeqCst), 2);
    assert_eq!(renderer.second_limit.load(Ordering::SeqCst), 100);
    assert_eq!(retained(&backend).await, 3);
    assert_eq!(
        stable_bytes(&model.request(4).messages[..prefix_len]),
        prefix,
        "a run's own render options keep the cached history"
    );
    core.shutdown().await.expect("the core shuts down");

    let core = standard_core(&backend, &model, &renderer, &tool, "second-build");
    send(&core, SESSION, "reopened", None).await;
    assert_eq!(renderer.first.load(Ordering::SeqCst), 2);
    assert_eq!(
        renderer.second.load(Ordering::SeqCst),
        2,
        "a restarted core renders nothing stored"
    );
    assert_eq!(
        retained(&backend).await,
        3,
        "a restarted core retains nothing again"
    );
    assert_eq!(
        stable_bytes(&model.request(6).messages[..prefix_len]),
        prefix,
        "a restarted core keeps the cached history"
    );
    core.shutdown().await.expect("the core shuts down");
    drop(keep);
}

// ---- the RLM protocol ----

/// A code renderer whose identity and counts switch with `mode`.
#[derive(Default)]
struct SwitchingCodeRenderer {
    mode: AtomicUsize,
    first: AtomicUsize,
    second: AtomicUsize,
}

impl lash::rlm::CodeRenderer for SwitchingCodeRenderer {
    fn id(&self) -> &str {
        if self.mode.load(Ordering::SeqCst) == 0 {
            "law.first"
        } else {
            "law.second"
        }
    }

    fn print(
        &self,
        value: &lashlang::Value,
        params: &lash::rlm::RenderParams,
    ) -> lash::render::Rendered<String> {
        if self.mode.load(Ordering::SeqCst) == 0 {
            self.first.fetch_add(1, Ordering::SeqCst);
        } else {
            self.second.fetch_add(1, Ordering::SeqCst);
        }
        lash::render::render(value, params)
    }
}

/// A code renderer that only counts its prints.
struct CountingCodeRenderer {
    prints: AtomicUsize,
}

impl lash::rlm::CodeRenderer for CountingCodeRenderer {
    fn id(&self) -> &str {
        "law.third"
    }

    fn print(
        &self,
        value: &lashlang::Value,
        params: &lash::rlm::RenderParams,
    ) -> lash::render::Rendered<String> {
        self.prints.fetch_add(1, Ordering::SeqCst);
        lash::render::render(value, params)
    }
}

fn rlm_script(call: usize, request: &LlmRequest) -> LlmResponse {
    let text = match call {
        0 => {
            "<typescript>\nlet saved = \"live value\"; print(\"ok\"); print(\"abcdefgh\");\n</typescript>"
        }
        1 => "first done",
        2 => "<typescript>\nprint(history[1].output[1]);\n</typescript>",
        3 => "second done",
        4 => "<typescript>\nprint(\"run spec\");\n</typescript>",
        5 => "run spec done",
        _ => "reopened done",
    };
    served::text(request, text)
}

fn print_cap(max_chars: usize) -> lash::rlm::RlmRenderPatch {
    lash::rlm::RlmRenderPatch {
        print: lash::rlm::RenderParamsPatch {
            max_chars: Some(max_chars),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// An RLM core whose prints `renderer` renders, at most `max_chars` each by
/// the deployment's default.
fn rlm_core(
    backend: &lash::Backend,
    model: &Arc<Model>,
    renderer: Arc<dyn lash::rlm::CodeRenderer>,
    max_chars: usize,
    build: &'static str,
) -> lash::LashCore {
    let mut config = lash::rlm::RlmProtocolPluginConfig::builder()
        .channel(lash::rlm::RlmChannel::Cell)
        .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
        .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
        .build();
    config.code_renderer = lash::rlm::CodeRendererSlot(renderer);
    config.render.print.max_chars = Some(max_chars);
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        config,
        Arc::new(lash::rlm::TypescriptDialect),
        backend,
    )
    .with_worker_service(sim::untimed_workers());
    lash::LashCore::rlm_builder(backend.clone(), factory)
        .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(model.provider(), served::metadata())
        .build(owner(build))
        .expect("the core builds")
}

/// An RLM session's stored prints keep the history cache prefix across a
/// render command with a renderer change, a run's own render options and a
/// restarted core whose renderer and default differ: a later cell's print is
/// cut by the options in force when it runs, and nothing stored is printed
/// again.
async fn stored_prints_keep_the_history_cache_prefix_across_renderer_change_and_reopen(tier: Tier) {
    const SESSION: &str = "rlm-render-cache";
    let Some((stores, keep)) = served::stores(tier).await else {
        return;
    };
    let backend = served::backend(stores);
    let model = Model::new(rlm_script);
    let renderer = Arc::new(SwitchingCodeRenderer::default());

    let core = rlm_core(
        &backend,
        &model,
        Arc::clone(&renderer) as Arc<dyn lash::rlm::CodeRenderer>,
        9,
        "first-build",
    );
    core.session(session_id(SESSION))
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            served::spec(64)
                .plugin(
                    lash::rlm::RLM_PROTOCOL_PLUGIN_ID,
                    lash::rlm::RlmCreateExtras {
                        render: Some(print_cap(5)),
                        ..Default::default()
                    },
                )
                .expect("the render options encode"),
        ))
        .await
        .expect("the session is created");
    send(&core, SESSION, "first", None).await;
    assert_eq!(renderer.first.load(Ordering::SeqCst), 2);
    assert_eq!(model.calls(), 2);
    let first_history = model.request(1);
    let prefix_len = history_prefix(&first_history).len();
    let prefix = stable_bytes(&history_prefix(&first_history));
    let preview_tail =
        serde_json::to_string(&first_history.messages[prefix_len..]).expect("preview tail JSON");
    assert!(preview_tail.contains("saved"), "{preview_tail}");
    assert!(
        String::from_utf8_lossy(&prefix).contains("within 5"),
        "the first cut is recorded in the stable history"
    );

    apply(
        &core,
        SESSION,
        lash::rlm::SetRlmRender {
            print: lash::rlm::RenderParamsPatch {
                max_chars: Some(3),
                ..Default::default()
            },
            preview: lash::rlm::RenderParamsPatch::default(),
        },
    )
    .await;
    renderer.mode.store(1, Ordering::SeqCst);
    send(&core, SESSION, "second", None).await;
    assert_eq!(renderer.first.load(Ordering::SeqCst), 2);
    assert_eq!(renderer.second.load(Ordering::SeqCst), 1);
    assert_eq!(
        stable_bytes(&model.request(2).messages[..prefix_len]),
        prefix,
        "a render command and a new renderer keep the cached history"
    );
    let latest_observation = model
        .request(3)
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            LlmContentBlock::Text { text, .. } => Some(text.to_string()),
            _ => None,
        })
        .find(|text| text.contains("history[4].output[0]:"))
        .expect("the second cell's observation");
    assert!(
        latest_observation.contains(
            "history[4].output[0]:\n[cut: 8 chars rendered within 3; chars 1; narrow with history[4].output[0].<path>]\nabc"
        ),
        "{latest_observation}"
    );

    send(
        &core,
        SESSION,
        "run-spec",
        Some(
            lash::runtime::ProtocolTurnOptions::typed(lash::rlm::RlmTurnOptions {
                render: Some(print_cap(2)),
                ..Default::default()
            })
            .expect("the run's render options encode"),
        ),
    )
    .await;
    assert_eq!(renderer.second.load(Ordering::SeqCst), 2);
    assert_eq!(
        stable_bytes(&model.request(4).messages[..prefix_len]),
        prefix,
        "a run's own render options keep the cached history"
    );
    assert!(format!("{:?}", model.request(5)).contains("within 2"));
    core.shutdown().await.expect("the core shuts down");

    let third = Arc::new(CountingCodeRenderer {
        prints: AtomicUsize::new(0),
    });
    let core = rlm_core(
        &backend,
        &model,
        Arc::clone(&third) as Arc<dyn lash::rlm::CodeRenderer>,
        2,
        "second-build",
    );
    send(&core, SESSION, "reopened", None).await;
    assert_eq!(
        stable_bytes(&model.request(6).messages[..prefix_len]),
        prefix,
        "a restarted core keeps the cached history"
    );
    assert_eq!(
        third.prints.load(Ordering::SeqCst),
        0,
        "a restarted core prints nothing stored"
    );
    core.shutdown().await.expect("the core shuts down");
    drop(keep);
}

mod sqlite_memory {
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn standard_runtime_keeps_recorded_history_across_params_renderer_and_reopen() {
        super::standard_runtime_keeps_recorded_history_across_params_renderer_and_reopen(
            super::Tier::SqliteMemory,
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn stored_prints_keep_the_history_cache_prefix_across_renderer_change_and_reopen() {
        super::stored_prints_keep_the_history_cache_prefix_across_renderer_change_and_reopen(
            super::Tier::SqliteMemory,
        )
        .await;
    }
}

mod sqlite_file {
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn standard_runtime_keeps_recorded_history_across_params_renderer_and_reopen() {
        super::standard_runtime_keeps_recorded_history_across_params_renderer_and_reopen(
            super::Tier::SqliteFile,
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn stored_prints_keep_the_history_cache_prefix_across_renderer_change_and_reopen() {
        super::stored_prints_keep_the_history_cache_prefix_across_renderer_change_and_reopen(
            super::Tier::SqliteFile,
        )
        .await;
    }
}
