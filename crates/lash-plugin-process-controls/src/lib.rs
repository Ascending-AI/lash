//! Protocol-stack runtime-control tools (`processes.list`,
//! `processes.cancel`, `processes.await`).
//!
//! Dedicated plugins register these tools into the normal tool-provider
//! surface, so protocol crates do not own or duplicate runtime control behavior.
//! A session installs them through [`SessionProcessAdminPluginFactory`]; a
//! process whose environment names [`PROCESS_CONTROLS_CONFIG`]'s component
//! gets the same tools through [`ProcessControlsComponent`] (FIG-6034).

#![deny(clippy::future_not_send)]

use std::sync::Arc;

use serde_json::Value;

use lash_core::ToolCall;
use lash_core::ToolOutcome;
use lash_core::ToolProvider;
use lash_core::plugin::{PluginFactory, PluginSpec, StaticPluginFactory};
use lash_tool_support::{StaticToolExecute, StaticToolProvider};
use lash_vocabulary::identity::ProcessId;
use lash_vocabulary::tool_contract::ToolDefinition;

mod declarations;

pub use declarations::{execute_process_get_tool_call, execute_process_start_tool_call};
use declarations::{process_get_tool_definition, process_start_tool_definition};

/// A process capability whose tool contract a host can inspect or grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessControlTool {
    Start,
    List,
    Await,
    Get,
    Cancel,
}

/// The canonical definition of one process-control tool.
pub fn process_tool_definition(tool: ProcessControlTool) -> ToolDefinition {
    match tool {
        ProcessControlTool::Start => process_start_tool_definition(),
        ProcessControlTool::List => process_list_tool_definition(),
        ProcessControlTool::Await => process_await_tool_definition(),
        ProcessControlTool::Get => process_get_tool_definition(),
        ProcessControlTool::Cancel => process_cancel_tool_definition(),
    }
}

/// Plugin factory for process-control tools.
///
/// Declares its provider through a [`PluginSpec`] executed by
/// [`StaticPluginFactory`], so it does not hand-roll the `SessionPlugin` +
/// `register` ceremony.
///
/// The [`LifetimePolicy`](lash_execution_contracts::process::LifetimePolicy) and the
/// [`ProcessEnvPolicy`](lash_core::ProcessEnvPolicy) are required and have no
/// default: they choose the lifetime, environment and engine settings of every
/// process the model's `start_process` declares, against the start's admitted
/// [`StartCx`](lash_execution_contracts::process::StartCx) and the attempt's recorded run. The model
/// never picks them (FIG-3607, FIG-6034).
pub struct SessionProcessAdminPluginFactory {
    inner: StaticPluginFactory,
}

impl SessionProcessAdminPluginFactory {
    /// Process controls whose starts take `lifetime`, for example
    /// [`lash_execution_contracts::process::lifetime::session_or_starter`], and the environment
    /// `environment` states, for example
    /// [`crate::policy::from_run`].
    pub fn new(
        lifetime: impl Fn(
            &lash_execution_contracts::process::StartCx,
        ) -> lash_execution_contracts::process::Lifetime
        + Send
        + Sync
        + 'static,
        environment: Arc<dyn lash_core::ProcessEnvPolicy>,
    ) -> Self {
        Self::with_cancel_process(Arc::new(lifetime), environment, true)
    }

    /// The same controls without `cancel_process`.
    pub fn without_cancel_process(
        lifetime: impl Fn(
            &lash_execution_contracts::process::StartCx,
        ) -> lash_execution_contracts::process::Lifetime
        + Send
        + Sync
        + 'static,
        environment: Arc<dyn lash_core::ProcessEnvPolicy>,
    ) -> Self {
        Self::with_cancel_process(Arc::new(lifetime), environment, false)
    }

    fn with_cancel_process(
        lifetime: lash_execution_contracts::process::LifetimePolicy,
        environment: Arc<dyn lash_core::ProcessEnvPolicy>,
        include_cancel_process: bool,
    ) -> Self {
        Self {
            inner: StaticPluginFactory::new(
                Self::declaration(),
                controls_spec(lifetime, environment, include_cancel_process),
            ),
        }
    }
}

/// The spec both twins install: one provider, so a process whose
/// environment names the controls starts children exactly as a session does.
fn controls_spec(
    lifetime: lash_execution_contracts::process::LifetimePolicy,
    environment: Arc<dyn lash_core::ProcessEnvPolicy>,
    include_cancel_process: bool,
) -> PluginSpec {
    let provider = Arc::new(StaticToolProvider::new(
        processes_tool_definitions(include_cancel_process),
        SessionProcessAdminTools {
            include_cancel_process,
            lifetime,
            environment,
        },
    )) as Arc<dyn ToolProvider>;
    let packages = lash_tool_support::ToolPackageProvider::new(
        CONTROLS_OWNER,
        CONTROLS_PROVIDER_KEY,
        [lash_vocabulary::names::Segment::new(CONTROLS_OWNER)
            .unwrap_or_else(|_| unreachable!("static process root"))]
        .into(),
        Arc::clone(&provider),
    );
    PluginSpec::new()
        .with_tool_provider(CONTROLS_PROVIDER_KEY, provider)
        .with_package_provider(CONTROLS_PROVIDER_KEY, Arc::new(packages))
}

const CONTROLS_OWNER: &str = "processes";
const CONTROLS_PROVIDER_KEY: lash_core::plugin::ToolProviderKey =
    lash_core::tool_provider_key!("tools");
/// The provider to link when a process environment installs process controls.
pub const PROCESS_CONTROLS_PROVIDER: lash_core::plugin::ToolProviderHandle =
    lash_core::plugin::ToolProviderHandle::literal(
        lash_core::plugin::RegistryKind::Process,
        CONTROLS_OWNER,
        CONTROLS_PROVIDER_KEY,
    );
/// The session plugin's provider; session and process addresses are distinct.
pub const SESSION_PROCESS_CONTROLS_PROVIDER: lash_core::plugin::ToolProviderHandle =
    lash_core::plugin::ToolProviderHandle::literal(
        lash_core::plugin::RegistryKind::Session,
        CONTROLS_OWNER,
        CONTROLS_PROVIDER_KEY,
    );

/// The config key of the process-controls component: its config is `()`.
pub const PROCESS_CONTROLS_CONFIG: lash_core::ProcessConfigKey<()> =
    lash_core::ProcessConfigKey::new(
        CONTROLS_OWNER,
        lash_vocabulary::providers::FormatVersion::ONE,
    );

/// The process controls as a process component (FIG-6034 §3.2): a process
/// whose environment names `processes` gets `start_process` and its siblings,
/// with the same required lifetime and environment policies as the session
/// twin.
pub struct ProcessControlsComponent {
    inner: lash_core::plugin::StaticProcessComponent,
}

impl ProcessControlsComponent {
    /// Process controls whose starts take `lifetime` and `environment`.
    pub fn new(
        lifetime: impl Fn(
            &lash_execution_contracts::process::StartCx,
        ) -> lash_execution_contracts::process::Lifetime
        + Send
        + Sync
        + 'static,
        environment: Arc<dyn lash_core::ProcessEnvPolicy>,
    ) -> Self {
        Self::with_cancel_process(Arc::new(lifetime), environment, true)
    }

    /// The same controls without `cancel_process`.
    pub fn without_cancel_process(
        lifetime: impl Fn(
            &lash_execution_contracts::process::StartCx,
        ) -> lash_execution_contracts::process::Lifetime
        + Send
        + Sync
        + 'static,
        environment: Arc<dyn lash_core::ProcessEnvPolicy>,
    ) -> Self {
        Self::with_cancel_process(Arc::new(lifetime), environment, false)
    }

    #[expect(
        clippy::expect_used,
        reason = "the controls spec fills only the provider, which a process component admits"
    )]
    fn with_cancel_process(
        lifetime: lash_execution_contracts::process::LifetimePolicy,
        environment: Arc<dyn lash_core::ProcessEnvPolicy>,
        include_cancel_process: bool,
    ) -> Self {
        let factory = StaticPluginFactory::new(
            SessionProcessAdminPluginFactory::declaration(),
            controls_spec(lifetime, environment, include_cancel_process),
        );
        Self {
            inner: factory
                .as_component(PROCESS_CONTROLS_CONFIG.format())
                .expect("the process controls are a process component"),
        }
    }
}

impl lash_core::plugin::ProcessComponentFactory for ProcessControlsComponent {
    fn declaration(&self) -> lash_core::plugin::ProcessComponentDeclaration {
        self.inner.declaration()
    }

    fn config_type(&self) -> std::any::TypeId {
        self.inner.config_type()
    }

    fn validate(
        &self,
        entry: &lash_core::ProcessComponentEntry,
        env: &lash_core::ProcessEnv,
    ) -> Result<(), lash_core::ProcessEnvRefusal> {
        self.inner.validate(entry, env)
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<
        lash_core::plugin::ProcessRecipe,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Arc::new(self.inner.clone()).recipe()
    }
}

impl PluginFactory for SessionProcessAdminPluginFactory {
    fn id(&self) -> &'static str {
        self.inner.id()
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<
        lash_core::plugin::PluginRecipe,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Arc::new(self.inner.clone()).recipe()
    }
}

impl SessionProcessAdminPluginFactory {
    pub fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(CONTROLS_OWNER)
    }
}
impl lash_core::plugin::PluginMetadata for SessionProcessAdminPluginFactory {
    fn plugin_declaration(&self) -> lash_core::plugin::PluginDeclaration {
        Self::declaration()
    }
}

struct SessionProcessAdminTools {
    include_cancel_process: bool,
    lifetime: lash_execution_contracts::process::LifetimePolicy,
    environment: Arc<dyn lash_core::ProcessEnvPolicy>,
}

#[async_trait::async_trait]
impl StaticToolExecute for SessionProcessAdminTools {
    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.name() == "processes.await" {
            return execute_process_await_tool_call(call.context, call.args);
        }
        if call.name() == "processes.start" {
            return execute_process_start_tool_call(
                call.context,
                call.args,
                &self.lifetime,
                self.environment.as_ref(),
            )
            .await;
        }
        if call.name() == "processes.get" {
            return execute_process_get_tool_call(call.context, call.args);
        }
        if call.name() == "processes.list" {
            return done_without_intents(
                execute_process_list_tool_call(call.context, call.args).await,
            );
        }
        if call.name() != "processes.cancel" || !self.include_cancel_process {
            return done_without_intents(ToolOutcome::err_fmt(format_args!(
                "Unknown leaf process tool: {}",
                call.name()
            )));
        }
        let process_id = match cancel_target(call.args) {
            Ok(process_id) => process_id,
            Err(refusal) => return done_without_intents(ToolOutcome::err_fmt(refusal)),
        };
        lash_core::ToolAttemptOutcome::done(
            lash_core::ToolOutcomeDone::ok(serde_json::json!({
                "process_id": process_id,
                "status": "cancelled",
            })),
            lash_execution_contracts::tool_intent::ToolIntents::v3(vec![
                lash_execution_contracts::tool_intent::ToolIntent::CancelProcess(
                    lash_execution_contracts::tool_intent::CancelProcessIntent {
                        owner: call.context.owner().runtime_owner(),
                        process_id,
                    },
                ),
            ]),
        )
    }
}

/// The process a cancel names, from either accepted spelling.
///
/// A handle is the shape every other process tool takes, so `cancel` accepts
/// it too and reads it through the one handle parser. The bare `process_id`
/// stays for a host that holds an id and never held a handle.
///
/// A value that is present but names no process is refused with the reason,
/// never passed over for the other spelling: a retired handle or an id no
/// registrar minted says so.
fn cancel_target(args: &Value) -> Result<ProcessId, String> {
    if let Some(handle) = args.get("handle") {
        return lash_core::process_id_from_handle_json(handle)
            .map_err(|refusal| format!("cancel_process `handle`: {refusal}"));
    }
    match args.get("process_id") {
        Some(Value::String(value)) => ProcessId::parse(value.trim())
            .map_err(|refusal| format!("cancel_process `process_id`: {refusal}")),
        Some(_) => Err("cancel_process `process_id` must be a string".to_string()),
        None => Err("cancel_process requires `handle` or `process_id`".to_string()),
    }
}

pub(crate) fn done_without_intents(result: ToolOutcome) -> lash_core::ToolAttemptOutcome {
    match result {
        ToolOutcome::Done(output) => lash_core::ToolAttemptOutcome::done_without_intents(
            lash_core::ToolOutcomeDone::from_output(*output),
        ),
        ToolOutcome::Pending(pending) => lash_core::ToolAttemptOutcome::pending(*pending),
    }
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
fn process_list_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:list_process_handles",
        "processes.list",
        "List process runs visible to this session, including host-launched runs, with process id, descriptor, optional definition ID, and lifecycle status. Filters are optional; the default returns running runs. Empty arguments select running runs; `definition` selects runs of an inline function and `definition_id` selects the ID from a returned handle or published workflow and `status: \"any\"` includes visible run history.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "status": {
                    "anyOf": [
                        { "const": "any" },
                        { "type": "object", "properties": { "in": { "type": "array", "items": { "enum": ["running", "waiting", "completed", "failed", "cancelled", "abandoned"] } } }, "required": ["in"], "additionalProperties": false }
                    ],
                    "description": "Any-of lifecycle status set. Absence selects running runs; `any` includes every status."
                },
                "definition": declarations::function_slot_schema(),
                "definition_id": declarations::definition_id_schema()
            },
            "additionalProperties": false
        }),
        process_list_output_schema(),
    ).expect("valid declared tool schemas")
    // One store read or write: a short body.
    .with_execution(std::time::Duration::from_secs(30))
}

fn processes_tool_definitions(include_cancel_process: bool) -> Vec<ToolDefinition> {
    let mut definitions = vec![
        process_start_tool_definition(),
        process_list_tool_definition(),
        process_await_tool_definition(),
        process_get_tool_definition(),
    ];
    if include_cancel_process {
        definitions.push(process_cancel_tool_definition());
    }
    definitions
}

/// `processes.await(handle)` — park until the process behind `handle` reaches
/// its terminal, and answer with that terminal.
///
/// The argument is typed through `x-lash` rather than as a record: a cell
/// passes the process handle value itself, whose nominal type is not assignable
/// to a record, so a `{"type":"object"}` parameter would refuse the call in the
/// type checker before the handler ever ran.
///
/// The kind is `process_unknown` — a process the host can only describe as
/// callable — because the host has no authoritative call signature for an
/// arbitrary awaited process. The argument uses the same nominal process
/// contract as the handle returned by a start.
#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
fn process_await_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:await_process",
        "processes.await",
        "Wait for a durable process to finish and return its terminal outcome. Pass the handle a process start or `processes.list(...)` returned. The wait is durable: it survives a restart of the waiting turn, and cancelling the turn drops the wait without cancelling the process.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": {
                    "x-lash": { "kind": "process_unknown" },
                    "description": "Process handle to wait on, as returned by a process start or `processes.list(...)`."
                }
            },
            "required": ["handle"],
            "additionalProperties": false
        }),
        serde_json::json!({
            "description": "The process's terminal outcome."
        }),
    ).expect("valid declared tool schemas")
    // One store read or write: a short body.
    .with_execution(std::time::Duration::from_secs(30))

    // `await_process` parks, so admission records that it may defer and the
    // runtime pre-derives the completion key its recorded attempt reads.
    // An await has no deadline of its own: it parks until the awaited
    // process ends or the awaiting scope does.
    .with_declaration(lash_vocabulary::tool_declaration::ToolDeclaration::deferring(), Some(lash_vocabulary::tool_contract::ParkBound::UntilScopeEnd))
    .expect("a deferring tool declares its park bound")
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
fn process_cancel_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:cancel_process",
        "processes.cancel",
        "Request cancellation for a durable process, including a running host-launched process. Pass the handle a process start or `processes.list(...)` returned, or the bare `process_id`. The result names the process and the cancellation this call requested; await the handle for the process's terminal.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": {
                    "x-lash": { "kind": "process_unknown" },
                    "description": "Process handle to cancel, as returned by a process start or `processes.list(...)`."
                },
                "process_id": {
                    "type": "string",
                    "description": "Process id, for a caller that holds the bare id rather than a handle."
                }
            },
            "additionalProperties": false
        }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "process_id": { "type": "string" },
                "status": {
                    "type": "string",
                    "enum": ["running", "completed", "failed", "cancelled"]
                }
            },
            "required": ["process_id", "status"],
            "additionalProperties": false
        }),
    ).expect("valid declared tool schemas")
    // One store read or write: a short body.
    .with_execution(std::time::Duration::from_secs(30))
    .with_declaration(lash_vocabulary::tool_declaration::ToolDeclaration::default()
            .with_intents([lash_vocabulary::ToolIntentKind::CancelProcess]), None)
            .expect("a valid tool declaration")
}

/// Parks the call on the terminal of the handle's process.
///
/// It takes the completion key first and names the process terminal as the
/// resolver, so the runtime — not this tool — is responsible for arming the
/// wait, both now and on every redrive of the parked turn. That is what makes
/// the wait durable: nothing here holds a future, a task, or a watcher that a
/// crash could lose.
///
/// Deliberately intent-free. A parking attempt cannot carry tool intents by
/// construction, and it needs none: the durable act is the journaled arming the
/// runtime performs from the declaration below, not a side effect this body
/// requests.
pub fn execute_process_await_tool_call(
    context: &lash_core::AttemptContext<'_>,
    args: &Value,
) -> lash_core::ToolAttemptOutcome {
    let Some(handle) = args.get("handle") else {
        return done_without_intents(ToolOutcome::err_fmt("await_process requires `handle`"));
    };
    let process_id = match lash_core::process_id_from_handle_json(handle) {
        Ok(process_id) => process_id,
        Err(err) => return done_without_intents(ToolOutcome::err_fmt(err)),
    };
    if let Err(err) = context.completion_key() {
        return done_without_intents(ToolOutcome::err_fmt(err));
    }
    let mut pending = lash_core::PendingCompletion::new().resolved_by_process_terminal(process_id);
    pending.on_cancel = lash_core::CancelHint::Ignore;
    lash_core::ToolAttemptOutcome::pending(pending)
}

pub async fn execute_process_list_tool_call(
    context: &lash_core::AttemptContext<'_>,
    args: &Value,
) -> ToolOutcome {
    let mut args = args.clone();
    if let Some(value) = args
        .as_object_mut()
        .and_then(|map| map.remove("definition"))
    {
        if args.get("definition_id").is_some() {
            return ToolOutcome::err_fmt("list accepts only one of definition or definition_id");
        }
        let request = match inline_callable_request(context, &value) {
            Ok(request) => request,
            Err(error) => return ToolOutcome::err_fmt(error),
        };
        let inline = match context
            .resolve_inline_start(
                lash_protocol_types::LASH_VM_ENGINE_KIND,
                request,
                serde_json::Map::new(),
            )
            .await
        {
            Ok(inline) => inline,
            Err(error) => return ToolOutcome::err_fmt(error),
        };
        args["definition_id"] = inline.target.definition_id.to_tagged_json();
    }
    let filter = match lash_execution_contracts::process::ProcessListFilter::decode(&args) {
        Ok(filter) => filter,
        Err(err) => return ToolOutcome::err_fmt(err),
    };
    let processes = context.processes();
    let result = processes.list_handles_filtered(&filter).await;
    match result {
        Ok(entries) => ToolOutcome::ok(serde_json::json!(entries)),
        Err(err) => ToolOutcome::err_fmt(err.to_string()),
    }
}

/// The one handle shape `processes.list` answers with.
///
/// Written from [`lash_execution_contracts::process::ProcessHandleView`]'s own serde shape rather than
/// from a reading of what a caller might want: the contract a cell type-checks
/// against and the record it actually receives are the same shape, and
/// `list_output_contract_matches_the_handle_view` fails if they drift. The
/// previous hand-written schema had already drifted — it advertised a
/// `descriptor` object and a `{name}` definition that no handle view has ever
/// carried.
fn process_list_output_schema() -> Value {
    serde_json::json!({
        "type": "array",
        "items": process_handle_view_schema()
    })
}

/// The schema of one [`lash_execution_contracts::process::ProcessHandleView`].
pub fn process_handle_view_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "__handle__": {
                "type": "string",
                "enum": ["lash"],
                "description": "Handle marker; pass the whole record where a process handle is needed."
            },
            "id": {
                "type": "string",
                "description": "Opaque handle id. Carry it and hand it back; do not read its parts or build one."
            },
            "process_id": {
                "type": "string",
                "description": "The process this handle names, for tools that ask for a process_id."
            },
            "kind": {
                "type": "string",
                "description": "Engine kind that owns the run."
            },
            "label": {
                "type": "string",
                "description": "Host-facing label, absent when the run has none."
            },
            "definition_id": declarations::definition_id_schema(),
            "status": {
                "type": "string",
                "enum": ["running", "waiting", "completed", "failed", "cancelled", "abandoned"]
            }
        },
        "required": ["__handle__", "id", "process_id", "kind", "status"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_output_contract_matches_the_handle_view() {
        // The contract a cell type-checks against and the record it receives
        // must be the same shape. Comparing the schema's property set against a
        // real `ProcessHandleView` serialization is what catches the drift the
        // previous hand-written schema had already accumulated.
        let view = lash_execution_contracts::process::ProcessHandleView::new(
            lash_vocabulary::identity::ProcessId::fixture("process-1"),
            {
                let mut identity = lash_execution_contracts::process::ProcessIdentity::labelled(
                    "lashvm",
                    Some("on_button"),
                );
                identity.definition_id =
                    Some(lash_core::ProcessDefinitionId::from_sha256_digest([1; 32]));
                identity
            },
            lash_execution_contracts::process::ProcessStatus::Running,
        );
        let serialized = serde_json::to_value(&view).expect("a handle view serializes");
        let serialized = serialized.as_object().expect("a handle view is an object");
        let schema = process_handle_view_schema();
        let properties = schema["properties"]
            .as_object()
            .expect("the schema declares properties");

        for name in serialized.keys() {
            assert!(
                properties.contains_key(name),
                "the handle view carries `{name}`, which the contract does not declare"
            );
        }
        for name in properties.keys() {
            assert!(
                serialized.contains_key(name),
                "the contract declares `{name}`, which no handle view carries"
            );
        }
        for name in schema["required"]
            .as_array()
            .expect("the schema names required properties")
        {
            let name = name.as_str().expect("a required property is a name");
            assert!(
                serialized.contains_key(name),
                "the contract requires `{name}`, which this handle view omits"
            );
        }
    }

    #[test]
    fn await_process_declares_a_deferring_attempt_and_a_handle_typed_argument() {
        let definition = process_await_tool_definition();
        assert!(
            definition.manifest.declaration().may_defer,
            "the runtime only pre-derives a completion key for a tool that declares it defers"
        );
        // A `{"type":"object"}` parameter would refuse a nominally typed cell
        // handle in the type checker before the handler ran (FIG-2989), which
        // is exactly what the `x-lash` keyword exists to avoid.
        assert_eq!(
            definition.contract.input_schema.canonical.as_value()["properties"]["handle"]["x-lash"],
            serde_json::json!({ "kind": "process_unknown" })
        );
    }

    /// A handle or id that is present but names no process is refused with
    /// its reason, never reported as a missing argument.
    #[test]
    fn a_cancel_target_that_names_no_process_is_refused_with_its_reason() {
        let retired = serde_json::json!({
            "handle": { "__handle__": "lash", "id": "p.1.old-name" },
            "process_id": lash_core::process_id_for_test("fallback").as_str(),
        });
        let refusal = cancel_target(&retired).expect_err("a retired handle names no process");
        assert!(refusal.contains("retired spelling"), "{refusal}");

        let unminted = serde_json::json!({ "process_id": "host-chosen-name" });
        let refusal = cancel_target(&unminted).expect_err("a host name is no minted id");
        assert!(
            refusal.starts_with("cancel_process `process_id`"),
            "{refusal}"
        );

        let refusal = cancel_target(&serde_json::json!({})).expect_err("nothing named");
        assert_eq!(refusal, "cancel_process requires `handle` or `process_id`");

        let minted = lash_core::process_id_for_test("minted");
        let named = serde_json::json!({ "process_id": minted.as_str() });
        assert_eq!(cancel_target(&named), Ok(minted));
    }
}

#[cfg(test)]
mod certification {
    use super::*;
    struct List;
    #[lash::async_trait]
    impl lash::testing::PluginScenario for List {
        fn callback(&self) -> lash::plugins::PluginCallbackIdentity {
            SESSION_PROCESS_CONTROLS_PROVIDER.callback_identity()
        }
        async fn prepare(
            &self,
            harness: &mut lash::testing::PluginHarness,
        ) -> Result<(), lash::testing::PluginHarnessError> {
            harness
                .create(lash::plugins::PluginOptions::default())
                .await
        }
        fn input(&self) -> lash::TurnInput {
            lash::TurnInput::text("list processes")
        }
        fn responses(&self) -> Vec<lash::provider::LlmResponse> {
            vec![
                lash::provider::LlmResponse {
                    parts: vec![lash::direct::LlmOutputPart::ToolCall {
                        call_id: "list".into(),
                        tool_name: "processes__list".into(),
                        input_json: "{}".into(),
                        replay: None,
                    }],
                    ..Default::default()
                },
                lash::testing::text_response("done"),
            ]
        }
        fn pause_after_responses(&self) -> usize {
            1
        }
    }
    /// FIG-6093: an executable process control retains its binding on reopen.
    #[tokio::test]
    async fn process_controls_pass_certification() {
        let factory = SessionProcessAdminPluginFactory::new(
            lash_execution_contracts::process::lifetime::session_or_starter,
            crate::policy::from_run(|_| Ok(Vec::new())),
        );
        let scenario: Arc<dyn lash::testing::PluginScenario> = Arc::new(List);
        let report = lash::testing::certify_plugin_with(
            Arc::new(factory),
            lash::testing::HostPolicy::standard(),
            &[scenario],
        )
        .await
        .unwrap();
        let failures: Vec<_> = report.failures().map(ToString::to_string).collect();
        assert!(failures.is_empty(), "{}", failures.join("\n"));
        assert!(report.reports.iter().any(|report| report.law
            == lash::testing::PluginLaw::ReplayInvokesNoRecordedCallback
            && report.result == lash::testing::PluginLawResult::Passed));
    }
}

/// The process-controls plugin's environment choices. These are host policy;
/// the engine independently validates every linked start and its known debt.
pub mod policy {
    use lash_core::{
        ProcessEnv, ProcessEnvChoice, ProcessEnvCx, ProcessEnvPolicy, ProcessEnvPolicyError,
        ProcessEnvRefusal, ProcessStartChoice,
    };
    use lash_core_store::process_env::ProcessComponentEntry;
    use lash_core_store::resolutions::{ResolutionEntry, ResolutionSeed};
    use lash_core_store::tool_surface::{ExecutionEnvironment, OperationRoute, UseBinding};
    use lash_vocabulary::{claims::*, package::*, providers::*};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn refused(reason: impl Into<String>) -> ProcessEnvPolicyError {
        ProcessEnvPolicyError::Refused(ProcessEnvRefusal::Policy {
            reason: reason.into(),
        })
    }
    fn twin(
        composition: &lash_core::plugin::ProcessComposition,
        provider: &ToolProviderHandle,
    ) -> Result<ToolProviderHandle, ProcessEnvPolicyError> {
        composition
            .registry
            .keys()
            .find(|candidate| {
                candidate.registry() == RegistryKind::Process
                    && candidate.owner() == provider.owner()
                    && candidate.key() == provider.key()
            })
            .cloned()
            .ok_or_else(|| {
                refused(format!(
                    "provider {provider} has no registered process twin"
                ))
            })
    }
    fn route(
        composition: &lash_core::plugin::ProcessComposition,
        source: &OperationRoute,
    ) -> Result<OperationRoute, ProcessEnvPolicyError> {
        let mut route = source.clone();
        route.provider = twin(composition, &route.provider)?;
        if let Some(payload) = &mut route.payload {
            payload.codec = twin(composition, &payload.codec)?;
        }
        Ok(route)
    }
    fn require(route: &OperationRoute, requirements: &mut ClaimRequirements) {
        requirements.providers.insert(ProviderName {
            owner: route.provider.owner().into(),
            key: route.provider.key().into(),
        });
        let callback = if route.policy == lash_core::ExecutionPolicy::Read {
            route.provider.reads_identity()
        } else {
            route.provider.callback_identity()
        };
        requirements.callbacks.insert(CallbackRequirement {
            registry: RegistryKind::Process,
            callback,
        });
        if let Some(payload) = &route.payload {
            requirements.codecs.insert((
                ProviderName {
                    owner: payload.codec.owner().into(),
                    key: payload.codec.key().into(),
                },
                payload.format,
            ));
            requirements.callbacks.insert(CallbackRequirement {
                registry: RegistryKind::Process,
                callback: payload.codec.callback_identity(),
            });
        }
    }
    async fn capture(
        cx: &ProcessEnvCx<'_>,
        components: Vec<ProcessComponentEntry>,
    ) -> Result<ProcessStartChoice, ProcessEnvPolicyError> {
        let source = cx
            .execution
            .or_else(|| cx.enclosing.map(|parent| parent.env.execution()))
            .ok_or_else(|| refused("this start has no recorded execution environment"))?;
        let mut builder = match cx.enclosing {
            Some(parent) => {
                let mut builder = ProcessEnv::builder(parent.env.max_tool_calls())
                    .attachments(Arc::clone(parent.env.attachments()));
                if let Some(model) = parent.env.model() {
                    builder = builder.model(model.clone());
                }
                for (slot, value) in parent.env.capabilities() {
                    builder = builder.capability(slot.clone(), value.clone());
                }
                builder
            }
            None => {
                let run = cx
                    .run
                    .ok_or_else(|| refused("this start has no recorded run"))?;
                let config = run.config();
                let mut builder = ProcessEnv::builder(config.max_tool_calls)
                    .attachments(Arc::clone(&config.attachment_acceptance));
                if let Some(model) = &config.model {
                    builder = builder.model(lash_core::ProcessModel::new(
                        model.clone(),
                        config.generation.clone(),
                        config.charge_safety.clone(),
                    ));
                }
                for (slot, value) in run.capabilities() {
                    builder = builder.capability(slot.clone(), value.clone());
                }
                builder
            }
        };
        for component in &components {
            builder = builder.entry(component.clone());
        }
        let mut execution = ExecutionEnvironment {
            publications: source.publications.clone(),
            authored: source.authored.clone(),
            roots: [cx.target.linked.link].into(),
            uses: BTreeMap::new(),
            registry: RegistryKind::Process,
            requirements: ClaimRequirements {
                machine: cx.target.linked.machine.clone(),
                ..Default::default()
            },
        };
        let skeleton = builder
            .execution(execution.clone())
            .build()
            .map_err(ProcessEnvPolicyError::Refused)?;
        let composition = cx
            .components()
            .compose(&skeleton, None)
            .map_err(ProcessEnvPolicyError::Refused)?;
        let mut initial_resolutions = Vec::new();
        for (use_, need) in &cx.target.linked.operations {
            let binding = source.uses.get(use_).ok_or_else(|| {
                refused(format!("operation use {use_:?} has no recorded binding"))
            })?;
            let binding = match binding {
                UseBinding::Fixed(fixed) => {
                    let mut fixed = fixed.clone();
                    fixed.route = route(&composition, &fixed.route)?;
                    require(&fixed.route, &mut execution.requirements);
                    UseBinding::Fixed(fixed)
                }
                UseBinding::Deferred(grant) => {
                    let mut grant = grant.clone();
                    let resolutions = cx
                        .enclosing
                        .map_or(cx.resolutions, |parent| &parent.resolutions);
                    if let Some(entry) = resolutions.get(use_) {
                        let entry = match entry {
                            ResolutionEntry::Resolved {
                                definition,
                                contract,
                                route: target,
                                mode,
                                requirements: _,
                            } => {
                                let target = route(&composition, target)?;
                                let name = match need { lash_execution_contracts::process::linking::OperationNeed::Deferred { name, .. } => name, _ => return Err(refused("deferred binding does not match selected Link")) };
                                let requirements = composition
                                    .composition
                                    .validate_resolved_route(
                                        &grant,
                                        name,
                                        definition,
                                        &target,
                                        RegistryKind::Process,
                                    )
                                    .map_err(|error| {
                                        ProcessEnvPolicyError::Package(Box::new(error))
                                    })?;
                                ResolutionEntry::Resolved {
                                    definition: definition.clone(),
                                    contract: contract.clone(),
                                    route: Box::new(target),
                                    mode: *mode,
                                    requirements,
                                }
                            }
                            entry => entry.clone(),
                        };
                        if let ResolutionEntry::Resolved { requirements, .. } = &entry {
                            execution
                                .requirements
                                .providers
                                .extend(requirements.providers.clone());
                            execution
                                .requirements
                                .codecs
                                .extend(requirements.codecs.clone());
                            execution
                                .requirements
                                .callbacks
                                .extend(requirements.callbacks.clone());
                            execution
                                .requirements
                                .components
                                .extend(requirements.components.clone());
                        }
                        initial_resolutions.push(ResolutionSeed { use_: *use_, entry });
                    } else {
                        let entry = components
                        .iter()
                        .find(|entry| entry.id().as_str() == grant.provider.owner)
                        .ok_or_else(|| {
                            refused(
                                "unresolved grant has no explicitly configured process component",
                            )
                        })?;
                        if entry.config().format_version() != grant.config.format
                            || entry.config().value() != &grant.config.value
                        {
                            return Err(refused(
                                "process resolver config differs from pinned grant config",
                            ));
                        }
                        grant.config.component = Some(entry.id().clone());
                        let known = composition
                            .composition
                            .validate_deferred_grant(&grant, RegistryKind::Process)
                            .map_err(|error| ProcessEnvPolicyError::Package(Box::new(error)))?;
                        execution.requirements.providers.extend(known.providers);
                        execution.requirements.codecs.extend(known.codecs);
                        execution.requirements.callbacks.extend(known.callbacks);
                        execution.requirements.components.extend(known.components);
                    }
                    UseBinding::Deferred(grant)
                }
            };
            execution.uses.insert(*use_, binding);
        }
        let mut builder = ProcessEnv::builder(skeleton.max_tool_calls())
            .attachments(Arc::clone(skeleton.attachments()));
        if let Some(model) = skeleton.model() {
            builder = builder.model(model.clone());
        }
        for component in skeleton.components() {
            builder = builder.entry(component.clone());
        }
        for (slot, value) in skeleton.capabilities() {
            builder = builder.capability(slot.clone(), value.clone());
        }
        let env = builder
            .execution(execution)
            .build()
            .map_err(ProcessEnvPolicyError::Refused)?;
        Ok(ProcessStartChoice {
            env: ProcessEnvChoice::New(env),
            engine_settings: match cx.enclosing {
                Some(parent) if parent.engine_kind == cx.target.engine_kind => {
                    parent.settings.clone()
                }
                _ => cx.engine_defaults()?,
            },
            initial_resolutions,
        })
    }

    type Mapping = dyn Fn(&ProcessEnvCx<'_>) -> Result<Vec<ProcessComponentEntry>, ProcessEnvRefusal>
        + Send
        + Sync;
    struct FromRun(Box<Mapping>);
    #[async_trait::async_trait]
    impl ProcessEnvPolicy for FromRun {
        async fn choose(
            &self,
            cx: &ProcessEnvCx<'_>,
        ) -> Result<ProcessStartChoice, ProcessEnvPolicyError> {
            capture(cx, (self.0)(cx).map_err(ProcessEnvPolicyError::Refused)?).await
        }
    }
    pub fn from_run(
        mapping: impl Fn(&ProcessEnvCx<'_>) -> Result<Vec<ProcessComponentEntry>, ProcessEnvRefusal>
        + Send
        + Sync
        + 'static,
    ) -> Arc<dyn ProcessEnvPolicy> {
        Arc::new(FromRun(Box::new(mapping)))
    }
    struct RestrictOr(Arc<dyn ProcessEnvPolicy>);
    #[async_trait::async_trait]
    impl ProcessEnvPolicy for RestrictOr {
        async fn choose(
            &self,
            cx: &ProcessEnvCx<'_>,
        ) -> Result<ProcessStartChoice, ProcessEnvPolicyError> {
            match cx.enclosing {
                Some(parent) => capture(cx, parent.env.components().to_vec()).await,
                None => self.0.choose(cx).await,
            }
        }
    }
    pub fn restrict_or(inner: Arc<dyn ProcessEnvPolicy>) -> Arc<dyn ProcessEnvPolicy> {
        Arc::new(RestrictOr(inner))
    }
    struct Published(lash_execution_contracts::process::EngineLaunch);
    #[async_trait::async_trait]
    impl ProcessEnvPolicy for Published {
        async fn choose(
            &self,
            _: &ProcessEnvCx<'_>,
        ) -> Result<ProcessStartChoice, ProcessEnvPolicyError> {
            Ok(ProcessStartChoice {
                env: ProcessEnvChoice::Published(self.0.env.clone()),
                engine_settings: self.0.settings.clone(),
                initial_resolutions: self.0.initial_resolutions.clone(),
            })
        }
    }
    pub fn published(
        launch: lash_execution_contracts::process::EngineLaunch,
    ) -> Arc<dyn ProcessEnvPolicy> {
        Arc::new(Published(launch))
    }
    struct Refuse(String);
    #[async_trait::async_trait]
    impl ProcessEnvPolicy for Refuse {
        async fn choose(
            &self,
            _: &ProcessEnvCx<'_>,
        ) -> Result<ProcessStartChoice, ProcessEnvPolicyError> {
            Err(refused(self.0.clone()))
        }
    }
    pub fn refuse(reason: impl Into<String>) -> Arc<dyn ProcessEnvPolicy> {
        Arc::new(Refuse(reason.into()))
    }
}

/// The tool accepts a function value; execution provenance comes only from its admitting context.
fn inline_callable_request(
    context: &lash_core::AttemptContext<'_>,
    value: &serde_json::Value,
) -> Result<lash_execution_contracts::process::InlineRequest, lash_core::ResolveTargetError> {
    let refused = |message: &str| lash_core::ResolveTargetError::Refused(message.into());
    let wrapper = value
        .as_object()
        .ok_or_else(|| refused("definition must be a function value"))?;
    if wrapper.len() != 1 {
        return Err(refused("definition must be a function value"));
    }
    let function = wrapper
        .get("$lash_function")
        .ok_or_else(|| refused("definition must be a function value"))?;
    let link = function
        .get("callable")
        .and_then(|callable| callable.get("link"))
        .ok_or_else(|| refused("function value has no defining Link"))?;
    let origin = context.callable_origin()?;
    context.engine_inline_request(serde_json::json!({"kind": "callable", "value": {"link": link, "function": function, "origin": origin}}))
}
