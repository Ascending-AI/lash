//! Where a spawn parents its child (ADR 0124 §6): a session parents its own
//! spawn, a process's spawn parents under its chain's originator session,
//! and a host-originated chain or a `ParentFork` capability inside a process
//! is refused.

use super::*;
use lash_core::runtime::RuntimeSessionState;
use lash_sansio::sync::MutexExt;
use serde_json::json;

/// Answers a named snapshot for any session and records which one was read.
#[derive(Default)]
struct NamedSnapshots {
    read: std::sync::Mutex<Vec<lash_core::SessionId>>,
}

#[async_trait::async_trait]
impl lash_core::plugin::runtime_host::SessionStateService for NamedSnapshots {
    async fn snapshot_session(
        &self,
        session_id: &lash_core::SessionId,
    ) -> Result<lash_core::SessionSnapshot, lash_core::PluginError> {
        self.read.lock_recover().push(session_id.clone());
        Ok(RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        ))
        .to_snapshot())
    }
}

struct SourcedCapability(lash_core::SessionPluginSource);

impl crate::Capability for SourcedCapability {
    fn name(&self) -> &str {
        "default"
    }

    fn build_session_request(
        &self,
        ctx: crate::SubagentSpawnContext<'_>,
    ) -> Result<lash_core::SessionCreateRequest, String> {
        ctx.rlm_request(self.name(), &SessionSpec::inherit(), self.0)
    }
}

fn provider(source: lash_core::SessionPluginSource) -> RlmSubagentToolsProvider {
    RlmSubagentToolsProvider {
        registry: Arc::new(CapabilityRegistry::new().with(Arc::new(SourcedCapability(source)))),
        session_spec: SessionSpec::inherit(),
        tool_access: SessionToolAccess::default(),
        final_answer_format: lash_rlm_types::RlmFinalAnswerFormat::RawFinalValue,
        parent_subagent: None,
        include_submit_error: false,
        lifetime: Arc::new(lash_core::lifetime::starter),
        timeout: None,
    }
}

fn spawn_call() -> PendingToolCall {
    PendingToolCall {
        call_id: lash_core::ToolCallId::fixture("spawn"),
        provider_call_id: None,
        tool_name: "spawn_agent".to_owned(),
        args: json!({ "task": "count the chunk" }),
        replay: None,
    }
}

fn process_owner() -> lash_core::RuntimeOwner {
    lash_core::RuntimeOwner::Process(lash_core::ProcessId::fixture("spawning-process"))
}

async fn prepare(
    source: lash_core::SessionPluginSource,
    owner: lash_core::RuntimeOwner,
    originator: Option<lash_core::ProcessOriginator>,
    sessions: Arc<NamedSnapshots>,
) -> Result<lash_core::PreparedToolCall, ToolOutcome> {
    let context = ToolPrepareContext::for_testing(owner, sessions, originator);
    provider(source)
        .prepare_spawn_agent(
            &lash_core::ToolId::from(SPAWN_AGENT_TOOL_ID),
            spawn_call(),
            &context,
        )
        .await
}

fn prepared_request(prepared: &lash_core::PreparedToolCall) -> lash_core::SessionCreateRequest {
    let payload: PreparedSpawnAgent =
        serde_json::from_value(prepared.prepared_payload.clone()).expect("prepared spawn payload");
    *payload.create_request
}

fn refusal_code(outcome: ToolOutcome) -> String {
    match outcome {
        ToolOutcome::Done(output) => match output.outcome {
            lash_core::ToolCallOutcome::Failure(failure) => failure.code,
            other => panic!("the spawn was refused as a failure: {other:?}"),
        },
        other => panic!("the spawn was refused inline: {other:?}"),
    }
}

#[tokio::test]
async fn a_process_spawn_parents_under_its_originator_session() {
    let sessions = Arc::new(NamedSnapshots::default());
    let prepared = prepare(
        lash_core::SessionPluginSource::CurrentHostFresh,
        process_owner(),
        Some(lash_core::ProcessOriginator::Session {
            session_id: lash_core::SessionId::from("originator"),
            agent_frame_id: None,
        }),
        Arc::clone(&sessions),
    )
    .await
    .unwrap_or_else(|outcome| panic!("the spawn prepares: {outcome:?}"));

    let request = prepared_request(&prepared);
    assert!(
        matches!(
            &request.relation,
            lash_core::SessionRelation::Child { parent_session_id, caused_by }
                if parent_session_id.as_str() == "originator"
                    && *caused_by == Some(lash_core::CausalRef::Process {
                        process_id: lash_core::ProcessId::fixture("spawning-process"),
                    })
        ),
        "the child parents under the originator, caused by the process: {:?}",
        request.relation
    );
    assert_eq!(
        *sessions.read.lock_recover(),
        vec![lash_core::SessionId::from("originator")],
        "the policy is read from the originator by name"
    );
}

#[tokio::test]
async fn a_host_originated_process_spawn_is_refused() {
    let sessions = Arc::new(NamedSnapshots::default());
    let refused = prepare(
        lash_core::SessionPluginSource::CurrentHostFresh,
        process_owner(),
        Some(lash_core::ProcessOriginator::host()),
        Arc::clone(&sessions),
    )
    .await
    .expect_err("a host-originated chain names no session to parent the child");

    assert_eq!(refusal_code(refused), crate::SPAWN_HOST_ORIGINATED_PROCESS);
    assert!(
        sessions.read.lock_recover().is_empty(),
        "no session stands in for the process"
    );
}

#[tokio::test]
async fn a_parent_fork_spawn_inside_a_process_is_refused() {
    let refused = prepare(
        lash_core::SessionPluginSource::ParentFork,
        process_owner(),
        Some(lash_core::ProcessOriginator::Session {
            session_id: lash_core::SessionId::from("originator"),
            agent_frame_id: None,
        }),
        Arc::new(NamedSnapshots::default()),
    )
    .await
    .expect_err("a process has no conversation to fork");

    assert_eq!(refusal_code(refused), crate::SPAWN_PARENT_FORK_IN_PROCESS);
}

#[tokio::test]
async fn a_session_spawn_parents_under_its_own_session() {
    let sessions = Arc::new(NamedSnapshots::default());
    let prepared = prepare(
        lash_core::SessionPluginSource::CurrentHostFresh,
        lash_core::RuntimeOwner::Session(lash_core::SessionId::from("spawner")),
        None,
        Arc::clone(&sessions),
    )
    .await
    .unwrap_or_else(|outcome| panic!("the spawn prepares: {outcome:?}"));

    assert!(matches!(
        &prepared_request(&prepared).relation,
        lash_core::SessionRelation::Child { parent_session_id, caused_by: Some(lash_core::CausalRef::ToolCall { .. }) }
            if parent_session_id.as_str() == "spawner"
    ));
    assert_eq!(
        *sessions.read.lock_recover(),
        vec![lash_core::SessionId::from("spawner")]
    );
}
