//! A code cell's process handles, through a host's `send()` on a served
//! node (FIG-5310, ported from the deleted laws of
//! lash-protocol-rlm's `executor/tests/deferred_and_processes.rs`).
//!
//! An RLM turn's cell creates and starts TypeScript processes; the core's
//! node runs each process's body on the production process steps, and the
//! cell awaits and inspects them through their handles.
//!
//! A process a cell starts may perform the tools that cell's deferred
//! resolution matched (FIG-6291): the start captures each from the cell's
//! committed match, with its binding, served by the process twins of the
//! match's provider and codec. It never resolves again.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash_core::ToolDefinitionBindingExt as _;
use lash_core::{ToolCall, ToolOutcome};
use lash_protocol_types::sync::MutexExt as _;

use served::{Tier, World};

/// The tool a cell hands a process id to.
const INSPECT: &str = "inspect";

fn inspect_definition() -> lash_core::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_core::ToolDefinition::raw(
        format!("tool:status_tool/{INSPECT}"),
        INSPECT,
        "Inspects the process the call names by id.",
        object.clone(),
        serde_json::json!({ "type": "string" }),
    )
    .expect("the inspector's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_core::ToolBinding::new(["status_tool"], INSPECT))
}

/// Records every process id it was handed.
#[derive(Default)]
struct Inspector {
    inspected: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Inspector {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash_core::PluginError> {
        Ok(vec![inspect_definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        (name == INSPECT).then(|| Arc::new(inspect_definition().contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let process_id = call.args["process_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        self.inspected.lock_recover().push(process_id);
        ToolOutcome::ok(serde_json::json!("inspected-ok")).into()
    }
}

/// A core running RLM turns whose processes outlive the turn that started
/// them, with the inspector.
async fn world(tier: Tier, inspector: &Arc<Inspector>) -> Option<World> {
    let inspector = Arc::clone(inspector);
    World::new(tier, move |backend| {
        lash::LashCore::rlm_builder(
            backend.clone(),
            served::rlm(backend, sim::untimed_workers()),
        )
        .plugin(Arc::new(
            lash::process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::detached,
                lash_core::runtime::env_policy::policy::from_run(|_| Ok(Vec::new())),
            ),
        ))
        .tools(lash::tool_provider_key!("tools"), inspector)
    })
    .await
}

/// Every process the core's registry holds.
async fn processes(world: &World) -> Vec<lash_core::ProcessRecord> {
    lash_core::testing::process_roster_records_for_fixture(
        world.backend.process_registry().as_ref(),
        &lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..lash_core::ProcessListFilter::default()
        },
    )
    .await
    .expect("the registry lists its processes")
}

/// A process handle a cell bound in one turn is still the process in the
/// session's next turn: that turn's cell awaits it and reads its terminal.
async fn typescript_restored_process_handle_await_crosses_turn_boundary(tier: Tier) {
    let inspector = Arc::new(Inspector::default());
    let Some(world) = world(tier, &inspector).await else {
        return;
    };
    world.script(
        "start-the-worker",
        vec![served::cell(
            "const worker = async () => { return \"done\"; };\n\
             const handle = await processes.start({ definition: worker });\n\
             await control.finish(\"started\");",
        )],
    );
    world.script(
        "await-the-worker",
        vec![served::cell("await control.finish(await handle);")],
    );
    let session = world.session("cross-turn-handle", served::spec(64)).await;
    let started = world.send(&session, "start-the-worker").await;
    served::assert_answered("the turn that starts the process", &started);
    assert_eq!(
        started.finished().map(|(_, value)| value),
        Some(&serde_json::json!("started"))
    );
    let awaited = world.send(&session, "await-the-worker").await;
    served::assert_answered("the next turn, which awaits the handle", &awaited);
    assert_eq!(
        awaited.finished().map(|(_, value)| value),
        Some(&serde_json::json!("done")),
        "the next turn's cell awaits the process the first turn started"
    );
    world.shutdown().await;
}

/// A cell reads its handle's `process_id` and hands it to a later call: the
/// tool receives the id of the process the cell started, and the cell goes
/// on with the tool's answer.
async fn typescript_cell_reads_process_handle_id_and_invokes_subsequent_operation(tier: Tier) {
    let inspector = Arc::new(Inspector::default());
    let Some(world) = world(tier, &inspector).await else {
        return;
    };
    let cell = served::cell(
        "const worker = async () => { return \"done\"; };\n\
         const handle = await processes.start({ definition: worker });\n\
         const processId = handle.process_id;\n\
         const status = await status_tool.inspect({ process_id: processId });\n\
         await control.finish({ id: processId, status: status });",
    );
    let output = world
        .run("handle-process-id", served::spec(64), vec![cell])
        .await;
    served::assert_answered("the turn that inspects its process", &output);
    let finish = output
        .finished()
        .map(|(_, value)| value)
        .expect("the cell finished with a value");
    let id = finish["id"].as_str().expect("the handle's id is a string");
    assert!(!id.is_empty(), "the handle's id is not empty");
    assert_eq!(finish["status"], serde_json::json!("inspected-ok"));
    assert_eq!(
        inspector.inspected.lock_recover().as_slice(),
        [id.to_owned()],
        "the tool received the id the cell read off its handle"
    );
    let records = processes(&world).await;
    assert_eq!(
        records
            .iter()
            .map(|record| record.id.to_string())
            .collect::<Vec<_>>(),
        [id.to_owned()],
        "the id is the started process's"
    );
    world.shutdown().await;
}

/// The component and session plugin whose provider `tools` executes the
/// tool the cell's deferred provider resolves: one owner and key in both
/// registries, so the session provider's process twin is its own.
const INBOX: lash::process::ProcessConfigKey<()> =
    lash::process::ProcessConfigKey::new("inbox", lash::plugins::FormatVersion::ONE);

/// The codec of a binding that is not the inbox's: its session provider
/// reads format 1 and its process twin only format 2.
const CODEC: lash::process::ProcessConfigKey<()> =
    lash::process::ProcessConfigKey::new("codec", lash::plugins::FormatVersion::ONE);

const FORMAT_TWO: lash::plugins::FormatVersion = match lash::plugins::FormatVersion::new(2) {
    Some(format) => format,
    None => lash::plugins::FormatVersion::ONE,
};

/// How long a started process sleeps before it sends: long enough for its
/// node to be replaced first.
const SLEEP_MS: u64 = 5_000;

/// The session provider `owner` declares as `tools`.
fn session_provider(owner: &str) -> lash::plugins::ToolProviderHandle {
    lash::plugins::ToolProviderHandle::new(
        lash::plugins::RegistryKind::Session,
        owner,
        lash::tool_provider_key!("tools"),
    )
}

/// `inbox.send_item`, which no provider advertises: the cell's deferred
/// provider resolves it, as `id`.
fn send_item(id: &str) -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        id,
        "send_item",
        "Sends one item.",
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["text"],
            "properties": { "text": { "type": "string" } },
        }),
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["route", "text"],
            "properties": {
                "route": { "type": "string" },
                "text": { "type": "string" },
            },
        }),
    )
    .expect("the tool's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_core::ToolBinding::new(["inbox"], "send_item"))
}

/// One body entry of `send_item`: what it sent, under which call, with the
/// route its binding named, on which node.
#[derive(Clone, Debug, PartialEq)]
struct Sent {
    text: String,
    call: lash_core::ToolCallId,
    route: serde_json::Value,
    node: usize,
}

/// What the laws watch: every send, every resolution, the node serving now
/// (node 2's resolver answers another tool and binding), the codec the
/// resolver binds with, and whether the start's env names the codec's twin.
struct Inbox {
    sent: Mutex<Vec<Sent>>,
    resolutions: AtomicUsize,
    node: AtomicUsize,
    codec: Mutex<&'static str>,
    twin_codec: AtomicBool,
}

impl Default for Inbox {
    fn default() -> Self {
        Self {
            sent: Mutex::default(),
            resolutions: AtomicUsize::new(0),
            node: AtomicUsize::new(1),
            codec: Mutex::new("inbox"),
            twin_codec: AtomicBool::new(false),
        }
    }
}

/// Executes `send_item` and reads bindings in `reads`; advertises nothing.
struct InboxTools {
    inbox: Arc<Inbox>,
    reads: &'static [lash::plugins::FormatVersion],
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for InboxTools {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash_core::PluginError> {
        Ok(Vec::new())
    }

    fn resolve_contract(
        &self,
        _name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        None
    }

    fn readable_binding_formats(&self) -> &'static [lash::plugins::FormatVersion] {
        self.reads
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let text = call.args["text"].as_str().unwrap_or_default().to_owned();
        let route = call
            .context
            .tool_execution_binding()
            .map_or(serde_json::Value::Null, |binding| {
                binding.payload["route"].clone()
            });
        self.inbox.sent.lock_recover().push(Sent {
            text: text.clone(),
            call: call.context.call_id().clone(),
            route: route.clone(),
            node: self.inbox.node.load(Ordering::SeqCst),
        });
        ToolOutcome::ok(serde_json::json!({ "route": route, "text": text })).into()
    }
}

/// Resolves `inbox.send_item` to the inbox's tool under a binding of the
/// codec the laws name; on node 2, to another tool and binding.
struct Resolver(Arc<Inbox>);

#[async_trait::async_trait]
impl lash_core::ToolProvider for Resolver {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash_core::PluginError> {
        Ok(Vec::new())
    }

    fn resolve_contract(
        &self,
        _name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        None
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        ToolOutcome::err_fmt(format_args!("Unknown tool: {}", call.name())).into()
    }
}

#[async_trait::async_trait]
impl lash::tools::DeferredToolProvider for Resolver {
    async fn resolve(
        &self,
        _cx: &lash::tools::DeferredResolveContext<'_>,
        paths: &[&str],
    ) -> BTreeMap<String, lash::tools::DeferredOutcome> {
        if !paths.contains(&"inbox.send_item") {
            return BTreeMap::new();
        }
        self.0.resolutions.fetch_add(1, Ordering::SeqCst);
        let (id, route) = match self.0.node.load(Ordering::SeqCst) {
            1 => ("tool:inbox/send_item", "first"),
            _ => ("tool:inbox/send_item_redeployed", "second"),
        };
        let binding = lash::tools::ExecutionBinding {
            codec: session_provider(*self.0.codec.lock_recover()),
            format: lash::plugins::FormatVersion::ONE,
            payload: serde_json::json!({ "route": route }),
        };
        BTreeMap::from([(
            "inbox.send_item".to_owned(),
            lash::tools::DeferredOutcome::Resolved {
                provider: session_provider("inbox"),
                definition: send_item(id),
                binding: Some(binding),
            },
        )])
    }
}

/// The session plugin, or with `reads` set the process component, `owner`
/// names, whose provider `tools` is the inbox's.
fn inbox_plugin(
    owner: &'static str,
    inbox: &Arc<Inbox>,
    reads: &'static [lash::plugins::FormatVersion],
) -> lash::plugins::StaticPluginFactory {
    lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(owner),
        lash::plugins::PluginSpec::new().with_tool_provider(
            lash::tool_provider_key!("tools"),
            Arc::new(InboxTools {
                inbox: Arc::clone(inbox),
                reads,
            }),
        ),
    )
}

/// A core whose cells resolve `inbox.send_item` through a deferred provider
/// and start processes under the run's captured env, naming the inbox,
/// the process controls and, when the laws say so, the codec's twin.
fn inbox_core(backend: &lash::Backend, inbox: &Arc<Inbox>) -> lash::LashCoreBuilder {
    const ONE: &[lash::plugins::FormatVersion] = &[lash::plugins::FormatVersion::ONE];
    const TWO: &[lash::plugins::FormatVersion] = &[FORMAT_TWO];
    let mapped = Arc::clone(inbox);
    let policy = lash_core::runtime::env_policy::policy::from_run(move |_| {
        let mut entries = vec![
            lash::process::ProcessComponentEntry::new(INBOX, &(), [])?,
            lash::process::ProcessComponentEntry::new(
                lash::process_controls::PROCESS_CONTROLS_CONFIG,
                &(),
                [],
            )?,
        ];
        if mapped.twin_codec.load(Ordering::SeqCst) {
            entries.push(lash::process::ProcessComponentEntry::new(CODEC, &(), [])?);
        }
        Ok(entries)
    });
    let component = |key: lash::process::ProcessConfigKey<()>, reads| {
        Arc::new(
            inbox_plugin(key.component(), inbox, reads)
                .as_component(key.format())
                .expect("a process component"),
        )
    };
    lash::LashCore::rlm_builder(
        backend.clone(),
        served::rlm(backend, sim::untimed_workers()),
    )
    .plugin(Arc::new(
        lash::process_controls::SessionProcessAdminPluginFactory::new(
            lash_core::lifetime::detached,
            policy,
        ),
    ))
    .plugin(Arc::new(inbox_plugin("inbox", inbox, ONE)))
    .plugin(Arc::new(inbox_plugin("codec", inbox, ONE)))
    .plugin(Arc::new(lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial("resolver"),
        lash::plugins::PluginSpec::new().with_deferred_provider(
            lash::tool_provider_key!("deferred"),
            lash::tools::DeferredToolProviderRegistration::new(Arc::new(Resolver(Arc::clone(
                inbox,
            )))),
        ),
    )))
    .process_component(component(INBOX, ONE), INBOX)
    .process_component(component(CODEC, TWO), CODEC)
    .process_component(
        Arc::new(lash::process_controls::ProcessControlsComponent::new(
            lash_core::lifetime::detached,
            lash_core::runtime::env_policy::policy::restrict_or(
                lash_core::runtime::env_policy::policy::refuse(
                    "a process starts only inside a process here",
                ),
            ),
        )),
        lash::process_controls::PROCESS_CONTROLS_CONFIG,
    )
}

/// A cell calls `inbox.send_item`, which its deferred provider resolves,
/// and starts an arrow that sleeps, sends, and starts a child that sends.
/// The start captures the cell's committed match and binding: the started
/// process parks on its sleep, its node is replaced by one whose resolver
/// now answers another tool and binding, and the process and its child,
/// restricted from its env, send there under the cell's binding, each under
/// its own call id. Nothing resolves again.
async fn started_process_captures_committed_deferred_binding(tier: Tier) {
    let inbox = Arc::new(Inbox::default());
    let built = Arc::clone(&inbox);
    let Some(mut world) = World::new(tier, move |backend| inbox_core(backend, &built)).await else {
        return;
    };
    let output = world
        .run(
            "deferred-start",
            served::spec(64),
            vec![served::cell(&format!(
                "const sent = await inbox.send_item({{ text: \"cell\" }});\n\
                 const worker = async () => {{\n  \
                   await sleep({SLEEP_MS});\n  \
                   const mine = await inbox.send_item({{ text: \"process\" }});\n  \
                   const started = await processes.start({{\n    \
                     definition: async () => await inbox.send_item({{ text: \"child\" }}),\n  \
                   }});\n  \
                   const theirs = await started;\n  \
                   return {{ mine: mine.route, theirs: theirs.route }};\n\
                 }};\n\
                 const handle = await processes.start({{ definition: worker }});\n\
                 await control.finish({{ sent: sent.route, process: handle.process_id }});"
            ))],
        )
        .await;
    served::assert_answered("the turn that starts the process", &output);
    let finish = output
        .finished()
        .map(|(_, value)| value.clone())
        .unwrap_or_else(|| {
            panic!(
                "the cell finished with a value; the model was sent: {:?}",
                world.requests("deferred-start").last()
            )
        });
    assert_eq!(finish["sent"], serde_json::json!("first"));
    let process = lash_core::ProcessId::parse(finish["process"].as_str().expect("a process id"))
        .expect("the handle names a process");

    inbox.node.store(2, Ordering::SeqCst);
    let rebuilt = Arc::clone(&inbox);
    world
        .restart("node-2", move |backend| inbox_core(backend, &rebuilt))
        .await;
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        world.core.processes().await_output(&process),
    )
    .await
    .expect("the process ends within two minutes")
    .expect("its end is read");
    let lash_core::ProcessAwaitOutput::Settled { output } = answer else {
        panic!("the process ended without an answer: {answer:?}");
    };
    assert!(output.is_success(), "{output:?}");
    assert_eq!(
        output.value_for_projection(),
        serde_json::json!({ "mine": "first", "theirs": "first" }),
        "the process and its child send under the binding the cell's match recorded"
    );
    let sent = inbox.sent.lock_recover().clone();
    assert_eq!(
        sent.iter()
            .map(|sent| (sent.text.as_str(), sent.route.clone(), sent.node))
            .collect::<Vec<_>>(),
        [
            ("cell", serde_json::json!("first"), 1),
            ("process", serde_json::json!("first"), 2),
            ("child", serde_json::json!("first"), 2),
        ],
        "each send ran once, the process's and the child's on the node that took the process over"
    );
    let calls = sent
        .iter()
        .map(|sent| sent.call.clone())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(calls.len(), 3, "each send is its own call: {sent:?}");
    assert_eq!(
        inbox.resolutions.load(Ordering::SeqCst),
        1,
        "only the cell resolved: no start, activation or child resolves again"
    );
    world.shutdown().await;
}

/// A start whose captured binding no env component can read is refused
/// typed, and no tool body runs: first with no process twin of the binding's
/// codec in the env, then with a twin that does not read its format.
async fn captured_deferred_codec_is_required_before_execution(tier: Tier) {
    let inbox = Arc::new(Inbox::default());
    *inbox.codec.lock_recover() = "codec";
    let built = Arc::clone(&inbox);
    let Some(world) = World::new(tier, move |backend| inbox_core(backend, &built)).await else {
        return;
    };
    let cell = served::cell(
        "const worker = async () => { return await inbox.send_item({ text: \"process\" }); };\n\
         let refused = \"\";\n\
         try { await processes.start({ definition: worker }); } catch (e) { refused = String(e); }\n\
         await control.finish(refused);",
    );
    for (session, twin, reason) in [
        (
            "codec-untwinned",
            false,
            "no component the env names declares it",
        ),
        ("codec-unreadable", true, "format 1"),
    ] {
        inbox.twin_codec.store(twin, Ordering::SeqCst);
        let output = world
            .run(session, served::spec(64), vec![cell.clone()])
            .await;
        served::assert_answered(session, &output);
        let refused = output
            .finished()
            .and_then(|(_, value)| value.as_str())
            .unwrap_or_default()
            .to_owned();
        assert!(
            refused.contains("inbox.send_item")
                && refused.contains("execution binding")
                && refused.contains(reason),
            "{session}: the start is refused for its binding's codec: {refused}"
        );
    }
    assert!(
        inbox.sent.lock_recover().is_empty(),
        "no tool body ran for a refused start"
    );
    assert!(processes(&world).await.is_empty(), "no process was started");
    world.shutdown().await;
}

tiered_laws!(
    typescript_restored_process_handle_await_crosses_turn_boundary,
    typescript_cell_reads_process_handle_id_and_invokes_subsequent_operation,
    started_process_captures_committed_deferred_binding,
    captured_deferred_codec_is_required_before_execution,
);
