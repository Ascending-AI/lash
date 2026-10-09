//! Where a spawn parents its child (ADR 0124 §6), what its create request
//! states, and which tools a session is offered by role. Ported from the
//! deleted subagent crate's `spawn_parent_tests` and `outcome_tests`.

use std::sync::{Arc, Mutex};

use lash::plugins::{
    AdmittedPluginConfig, PluginHost, PluginOptions, PluginSessionRequest, SessionAuthorityContext,
    SessionStateService, SessionToolAccess,
};
use lash::tools::{PendingToolCall, StaticToolExecute, ToolCallOutcome, ToolOutcome};
use lash::{MaxToolCalls, ProcessId, RuntimeOwner, SessionId, SessionSpec, ToolCallId, TurnBudget};
use serde_json::json;

use super::*;
use crate::tool::{PreparedSpawn, SpawnAgent};

/// Records every session a spawn asks a snapshot of. The example reads none.
#[derive(Default)]
struct NoSnapshots {
    read: Mutex<Vec<SessionId>>,
}

#[lash::async_trait]
impl SessionStateService for NoSnapshots {
    async fn snapshot_session(
        &self,
        session_id: &SessionId,
    ) -> Result<lash::runtime::SessionSnapshot, PluginError> {
        self.read
            .lock()
            .expect("the record lock")
            .push(session_id.clone());
        Err(PluginError::Session("a spawn reads no session".to_string()))
    }
}

fn child_spec() -> SessionSpec {
    SessionSpec::new("child-model", TurnBudget::bounded(3), MaxToolCalls::new(16))
        .no_progress_budget(lash::NoProgressBudget::bounded(12))
}

fn spawner() -> SpawnAgent {
    SpawnAgent {
        child: Arc::new(ChildConfig {
            spec: child_spec(),
            tool_access: SessionToolAccess::ambient()
                .with_hidden_tools(["write_file"])
                .expect("a hidden tool name"),
            prompt_plan: None,
            rlm: true,
            lifetime: Arc::new(lash::process::lifetime::starter),
        }),
    }
}

fn spawn_call() -> PendingToolCall {
    PendingToolCall {
        call_id: ToolCallId::fixture("spawn"),
        provider_call_id: None,
        tool_name: "spawn_agent".to_owned(),
        args: json!({ "task": "count the chunk", "output": { "len": "int" } }),
        replay: None,
    }
}

fn process_owner() -> RuntimeOwner {
    RuntimeOwner::Process(ProcessId::fixture("spawning-process"))
}

async fn prepare(
    owner: RuntimeOwner,
    originator: Option<lash::process::ProcessOriginator>,
    sessions: Arc<NoSnapshots>,
) -> Result<lash::tools::PreparedToolCall, ToolOutcome> {
    let context = lash::tools::ToolPrepareContext::for_testing(owner, sessions, originator);
    spawner()
        .prepare_tool_call(
            &lash::tools::ToolId::from("tool:spawn_agent"),
            spawn_call(),
            &context,
        )
        .await
}

fn prepared(prepared: &lash::tools::PreparedToolCall) -> PreparedSpawn {
    serde_json::from_value(prepared.prepared_payload.clone()).expect("prepared spawn payload")
}

fn refusal_code(outcome: ToolOutcome) -> String {
    match outcome {
        ToolOutcome::Done(output) => match output.outcome {
            ToolCallOutcome::Failure(failure) => failure.code,
            other => panic!("the spawn was refused as a failure: {other:?}"),
        },
        other => panic!("the spawn was refused inline: {other:?}"),
    }
}

#[tokio::test]
async fn a_process_spawn_parents_under_its_originator_session() {
    let sessions = Arc::new(NoSnapshots::default());
    let spawn = prepare(
        process_owner(),
        Some(lash::process::ProcessOriginator::Session {
            session_id: SessionId::from("originator"),
            agent_frame_id: None,
        }),
        Arc::clone(&sessions),
    )
    .await
    .unwrap_or_else(|outcome| panic!("the spawn prepares: {outcome:?}"));

    let request = prepared(&spawn).create_request;
    assert!(
        matches!(
            &request.relation,
            lash::persistence::SessionRelation::Child { parent_session_id, caused_by }
                if parent_session_id.as_str() == "originator"
                    && *caused_by == Some(lash::process::CausalRef::Process {
                        process_id: ProcessId::fixture("spawning-process"),
                    })
        ),
        "the child parents under the originator, caused by the process: {:?}",
        request.relation
    );
    assert!(
        sessions.read.lock().expect("the record lock").is_empty(),
        "the parent is a link only: no session is read"
    );
}

#[tokio::test]
async fn a_host_originated_process_spawn_is_refused() {
    let sessions = Arc::new(NoSnapshots::default());
    let refused = prepare(
        process_owner(),
        Some(lash::process::ProcessOriginator::host()),
        Arc::clone(&sessions),
    )
    .await
    .expect_err("a host-originated chain names no session to parent the child");

    assert_eq!(refusal_code(refused), SPAWN_HOST_ORIGINATED_PROCESS);
    assert!(sessions.read.lock().expect("the record lock").is_empty());
}

#[tokio::test]
async fn a_session_spawn_parents_under_its_own_session() {
    let spawn = prepare(
        RuntimeOwner::Session(SessionId::from("spawner")),
        None,
        Arc::new(NoSnapshots::default()),
    )
    .await
    .unwrap_or_else(|outcome| panic!("the spawn prepares: {outcome:?}"));

    assert!(matches!(
        &prepared(&spawn).create_request.relation,
        lash::persistence::SessionRelation::Child {
            parent_session_id,
            caused_by: Some(lash::process::CausalRef::ToolCall { .. })
        } if parent_session_id.as_str() == "spawner"
    ));
}

/// The child's create request states exactly the host's configuration and
/// the call's arguments: the host's spec (key unminted, its budgets), the
/// configured tool access, the plugin's own namespace with the task, and
/// the RLM termination with the call's output shape. The child process's
/// session id is the one its start derives.
#[tokio::test]
async fn a_childs_request_states_only_the_hosts_configuration_and_the_call() {
    let spawn = prepare(
        RuntimeOwner::Session(SessionId::from("spawner")),
        None,
        Arc::new(NoSnapshots::default()),
    )
    .await
    .unwrap_or_else(|outcome| panic!("the spawn prepares: {outcome:?}"));
    let spawn = prepared(&spawn);
    let request = *spawn.create_request;

    let expected = lash::SessionCreateRequest::root(
        lash::plugins::SessionToolAccess::ambient(),
        lash::SessionStartPoint::Empty,
        PluginOptions::default(),
    )
    .with_spec(&child_spec())
    .expect("the child spec states a session");
    assert_eq!(request.session_id, None);
    assert_eq!(request.policy, expected.policy);
    assert_eq!(request.model, expected.model);
    assert_eq!(request.reasoning, None);
    assert_eq!(request.prompt_plan, None);
    assert_eq!(request.tool_access, spawner().child.tool_access);
    assert_eq!(
        request.plugin_options.plugins[DELEGATION_PLUGIN_ID].value,
        json!({ "task": "count the chunk" })
    );
    let rlm: lash::rlm::RlmCreateExtras = serde_json::from_value(
        request.plugin_options.plugins[lash::rlm::RLM_PROTOCOL_PLUGIN_ID]
            .value
            .clone(),
    )
    .expect("the RLM extras decode");
    assert!(matches!(
        rlm.termination,
        Some(lash::rlm::RlmTermination::FinishRequired { schema: Some(_) })
    ));
    assert!(spawn.output_schema.is_some());
}

/// The tools a session built under `namespace` is offered.
fn offered(namespace: Option<DelegatedChild>) -> Vec<String> {
    let factory: Arc<dyn PluginFactory> = Arc::new(DelegationPluginFactory::new(
        lash::plugins::SessionToolAccess::ambient(),
        child_spec(),
        lash::process::lifetime::starter,
    ));
    let host = PluginHost::new(
        vec![
            Arc::new(lash::plugins::StandardProtocolPluginFactory::new()),
            factory,
        ],
        lash::ExecutionBudgets::recommended(),
        lash::runtime::TraceRuntime::new(std::sync::Arc::new(lash::runtime::SystemClock)),
    );
    let options = match namespace {
        Some(child) => PluginOptions::typed(DELEGATION_PLUGIN_ID, child).expect("the namespace"),
        None => PluginOptions::default(),
    };
    let config = host
        .resolve_creation_plugin_config(
            Some(lash::standard::STANDARD_PROTOCOL_PLUGIN_ID),
            &options,
            &lash::persistence::PluginAdmission::default(),
        )
        .expect("the creation config resolves");
    let session = host
        .build_session(PluginSessionRequest::creation(
            "offered",
            SessionAuthorityContext {
                plugin_config: AdmittedPluginConfig::new(config, 0),
                ..SessionAuthorityContext::ambient_fixture()
            },
        ))
        .expect("the session builds");
    session
        .tool_catalog()
        .expect("the catalog")
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect()
}

/// A session this plugin did not create may delegate. A delegated child,
/// which its own namespace marks, answers its task: it is offered
/// `submit_error` and never `spawn_agent`, so delegation cannot recurse.
/// The role is the plugin's own record, never the session's parent link.
#[test]
fn a_delegated_child_may_fail_its_task_and_may_not_delegate() {
    let delegator = offered(None);
    assert!(
        delegator.iter().any(|name| name == "spawn_agent"),
        "{delegator:?}"
    );
    assert!(
        !delegator.iter().any(|name| name == "submit_error"),
        "{delegator:?}"
    );
    let child = offered(Some(DelegatedChild {
        task: "count".to_string(),
    }));
    assert!(child.iter().any(|name| name == "submit_error"), "{child:?}");
    assert!(!child.iter().any(|name| name == "spawn_agent"), "{child:?}");
}

/// FIG-1480: an authored example the dialect's line rewriter cannot dress
/// as TypeScript reaches the model as a broken program. Pin the tool's real
/// examples at the rendered surface: respelled through the dialect, then
/// parsed.
#[test]
fn spawn_agent_examples_render_as_parseable_typescript() {
    use lash::rlm::Dialect as _;

    let definition = spawn_agent_tool_definition();
    let examples = &definition.contract().examples;
    let rendered = examples
        .iter()
        .map(|example| {
            lash::rlm::TypescriptDialect
                .render_tool_example(example)
                .expect("TypeScript spells every authored example")
        })
        .collect::<Vec<_>>();
    // Parsed rather than linked: examples name host modules and free
    // identifiers no isolated environment has, so an unknown binding is
    // expected and a *syntax* error is not.
    let mut unparseable = Vec::new();
    for (example, rendered) in examples.iter().zip(&rendered) {
        if let Err(error) = lash::typescript::parse(rendered) {
            let code = format!("{:?}", error.code);
            if code.contains("UnknownBinding") || code.contains("LinkError") {
                continue;
            }
            unparseable.push(format!("`{example}` -> `{rendered}`: {error}"));
        }
    }
    assert!(unparseable.is_empty(), "{unparseable:#?}");
    assert!(
        rendered.iter().any(|example| example
            .contains(r#"const Shape = { name: "str", tags: "list[str]", status: "str" };"#)),
        "{rendered:#?}"
    );
}
