//! A workflow process performs a component's `Read` tool (FIG-6273).
//!
//! A process component may supply a `Read` tool with the reader its
//! provider registers, as a session plugin may. A workflow process's
//! `perform` of it is a linked effect like any other: admitted and executed
//! as a durable process step, whose result is stable across replay. Only a
//! code cell reads inline (ADR 0144), since a cell reruns as a whole.

use super::*;

/// The component that contributes `tools.lookup`.
const KNOWLEDGE: lash_core::ProcessConfigKey<()> =
    lash_core::ProcessConfigKey::new("core-node-knowledge", lash_core::FormatVersion::ONE);

/// The lookup tool's id: what its step's admission names. Its effect is
/// `tools.lookup`.
const LOOKUP_TOOL: &str = "core-node-knowledge-lookup";

/// `lookup()`: one `perform` of the knowledge component's `Read` tool.
const LOOKUP: &str = r#"kernel 1
numbers float
effect tools.lookup(input: Json) -> Json contract "1"
entry lookup() -> Any

fn lookup() {
  let input = {topic: "lash"}
  let got = perform tools.lookup(input) as Any
  return got
}

main {
  finish null
}
"#;

/// The `Read` tool `lookup`: what the knowledge base says about a topic.
fn lookup_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        LOOKUP_TOOL,
        "tools.lookup",
        "Looks a topic up.",
        serde_json::json!({ "type": "object" }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("the lookup tool's schemas")
    // The contract the laws' kernel texts pin.
    .declared_contract(lash_core::ContractId::new("1"))
    .with_execution(Duration::from_secs(120))
    .with_execution_policy(lash_core::ExecutionPolicy::Read)
}

/// What the knowledge base says about the topic `args` names.
fn looked_up(args: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "topic": args["topic"], "says": "durable" })
}

/// How often the lookup's body and its reader answered.
#[derive(Default)]
struct Answered {
    bodies: AtomicUsize,
    reads: AtomicUsize,
}

struct LookupBody(Arc<Answered>);

#[async_trait::async_trait]
impl lash::tools::StaticToolExecute for LookupBody {
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.0.bodies.fetch_add(1, Ordering::SeqCst);
        lash_core::ToolOutcome::ok(looked_up(call.args)).into()
    }
}

struct LookupReads(Arc<Answered>);

#[async_trait::async_trait]
impl lash::tools::ToolReads for LookupReads {
    async fn read(
        &self,
        call: lash::tools::ReadCall<'_>,
    ) -> Result<serde_json::Value, lash::tools::ReadError> {
        self.0.reads.fetch_add(1, Ordering::SeqCst);
        Ok(looked_up(call.args))
    }
}

/// The knowledge component as a host declares it: a session plugin's spec
/// whose provider registers its reader, offered as a process component.
fn knowledge_component(
    answered: &Arc<Answered>,
) -> Arc<dyn lash::process::ProcessComponentFactory> {
    let provider = Arc::new(lash::tools::StaticToolProvider::new(
        vec![lookup_definition()],
        LookupBody(Arc::clone(answered)),
    ));
    Arc::new(
        lash_core::plugin::StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial(KNOWLEDGE.component()),
            lash_core::facade_support::PluginSpec::new().with_tool_provider_registration(
                lash::tool_provider_key!("tools"),
                lash::tools::ToolProviderRegistration::new(provider)
                    .with_reads(Arc::new(LookupReads(Arc::clone(answered)))),
            ),
        )
        .as_component(KNOWLEDGE.format())
        .expect("a provider with its reader is a process component"),
    )
}

/// PROCESS-PERFORMS-A-READ-TOOL: a kernel process whose environment links a
/// component's `Read` tool performs it as an ordinary durable step: the
/// step is admitted, its tool runs once, and the process ends with its
/// answer.
async fn a_workflow_process_performs_a_components_read_tool_as_a_step(tier: Tier) {
    let answered = Arc::new(Answered::default());
    let component = knowledge_component(&answered);
    let deployment = deploy_with(tier, Vec::new(), move |backend| {
        rlm_core(backend).process_component(component, KNOWLEDGE)
    })
    .await;
    let payload = kernel_process::payload(&deployment.backend, LOOKUP, "lookup").await;
    let process = start_under(
        &deployment.core,
        &environment_of(vec![(KNOWLEDGE, lookup_definition())], &[KNOWLEDGE]),
        lash_vm_runtime::LASH_VM_ENGINE_KIND,
        payload,
        |request| request,
    )
    .await;
    let output = ended(&deployment.core, &process).await;
    assert_eq!(
        success(&output),
        looked_up(&serde_json::json!({ "topic": "lash" }))
    );
    assert_eq!(
        (
            answered.bodies.load(Ordering::SeqCst),
            answered.reads.load(Ordering::SeqCst)
        ),
        (1, 0),
        "the perform ran as one step, not an inline read"
    );
    let facts = deployment
        .backend
        .durable()
        .run_facts(&lash_durable::domain::OwnerKey::Process(process))
        .await
        .expect("the process's run facts are read");
    let tool = LOOKUP_TOOL.as_bytes();
    assert!(
        facts
            .nodes
            .values()
            .any(|node| node.bytes.windows(tool.len()).any(|window| window == tool)),
        "the lookup's step is recorded: {facts:?}"
    );
}

on_every_tier!(a_workflow_process_performs_a_components_read_tool_as_a_step);
