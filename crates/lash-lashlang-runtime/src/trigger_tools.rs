//! `triggers.register` — the declaring leaf tool.
//!
//! Registration is an ordinary leaf tool now (FIG-3116), matching the
//! process-control pattern of ADR 0095: the attempt validates the
//! registration record and prepares the subscription draft, declares a
//! [`ToolIntent::RegisterTrigger`], and performs no durable act. The
//! subscription installs when the intent is realized, behind the attempt's
//! own commit.
//!
//! The attempt cannot answer a trigger handle directly: the receipt —
//! revision, fingerprint, the rest of the record — exists only once the
//! registration is realized, so it answers a result slot the realization
//! replaces (the same ADR 0107 mechanism `processes.start` uses).

use serde_json::Value;

use lash_core::{
    AttemptContext, ToolAttemptOutcome, ToolCall, ToolDefinition, ToolIntent, ToolIntents,
    ToolOutcome, ToolOutcomeDone,
};
use lash_tool_support::{StaticToolExecute, StaticToolProvider, ToolDefinitionBindingExt};

use crate::trigger_commands::prepare_trigger_draft;

/// The `triggers.register` tool definition: the record the retired resource
/// operation accepted, answered by the trigger handle its realization mints.
#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
pub fn register_trigger_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        lashlang::REGISTER_TRIGGER_TOOL_ID,
        "register_trigger",
        "Register a durable trigger subscription: the source emits occurrences \
         that start the target process with the declared inputs.",
        lashlang::register_trigger_tool_input_schema(),
        lashlang::register_trigger_tool_output_schema(),
    ).expect("valid declared tool schemas")
    // One store read or write: a short body.
    .with_execution(std::time::Duration::from_secs(30))
    .with_examples(vec![
        r#"await triggers.register({ source: timer.Schedule({ expr: "0 8 * * *" }), target: { definition: scan }, inputs: (event) => ({ tick: event }) })"#
            .into(),
    ])
    .with_declaration(
        lash_core::ToolDeclaration::default()
            .with_intents([lash_core::ToolIntentKind::RegisterTrigger]),
    )
    .with_tool_binding(lash_core::ToolBinding::new(["triggers"], "register"))
}

/// The provider a plugin registers to put `register_trigger` on the tool
/// surface. The artifact store is the module store the draft preparation
/// resolves the target's authoritative signature against.
pub fn register_trigger_tool_provider(
    workers: lash_vm_client::service::Service,
    artifact_store: lashlang::LashlangArtifacts,
) -> StaticToolProvider<RegisterTriggerTools> {
    StaticToolProvider::new(
        vec![register_trigger_tool_definition()],
        RegisterTriggerTools {
            artifact_store,
            workers,
        },
    )
}

pub struct RegisterTriggerTools {
    workers: lash_vm_client::service::Service,
    artifact_store: lashlang::LashlangArtifacts,
}

#[async_trait::async_trait]
impl StaticToolExecute for RegisterTriggerTools {
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        execute_register_trigger_tool_call(
            &self.workers,
            call.context,
            call.args,
            &self.artifact_store,
        )
        .await
    }
}

pub async fn execute_register_trigger_tool_call(
    workers: &lash_vm_client::service::Service,
    context: &AttemptContext<'_>,
    args: &Value,
    artifact_store: &lashlang::LashlangArtifacts,
) -> ToolAttemptOutcome {
    let request = match lashlang::TriggerRegistrationRequest::decode(args) {
        Ok(request) => request,
        Err(error) => return refuse(error.to_string()),
    };
    let prepared = match prepare_trigger_draft(
        workers,
        artifact_store,
        context.definition_engines(),
        &request,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(error) => return refuse(error.to_string()),
    };
    let owner = context.owner().runtime_owner();
    let session_scope = match context.owner() {
        lash_core::ExecutionOwner::SessionFrame {
            session_id,
            agent_frame_id,
        } => Some(lash_core::SessionScope::for_agent_frame(
            session_id.clone(),
            agent_frame_id.clone(),
        )),
        lash_core::ExecutionOwner::Process { .. } => None,
    };
    // The subscription belongs to the authority that owns the caller: a
    // registration declared inside a durable process carries the chain's
    // originator and its wake target, and a session's own declaration carries
    // the session's. Same ruling the host-operation path applied (FIG-3116).
    let provenance = context.process_spawn_provenance().cloned();
    let actor = match (provenance.as_ref(), session_scope.as_ref()) {
        (Some(spawn), _) => spawn.originator.clone(),
        (None, Some(session_scope)) => lash_core::ProcessOriginator::session(session_scope.clone()),
        (None, None) => {
            return refuse(format!(
                "{owner} carries no spawn provenance for the trigger it registers"
            ));
        }
    };
    let owner_scope = match lash_core::resolve_trigger_owner_scope(
        &owner,
        provenance.as_ref().map(|spawn| &spawn.originator),
    ) {
        Ok(scope) => scope,
        Err(error) => return refuse(error.to_string()),
    };
    let wake_target = provenance
        .as_ref()
        .and_then(|spawn| spawn.wake_session_id.as_ref())
        .map(|session| lash_core::SessionScope::new(session.clone()))
        .or(session_scope);
    let env_ref = match context.process_execution_env_ref() {
        Ok(env_ref) => env_ref,
        Err(error) => return refuse(error.to_string()),
    };
    let draft = match prepared.into_draft(env_ref, wake_target) {
        Ok(draft) => draft,
        Err(error) => return refuse(error.to_string()),
    };
    let identity = context.intent_identity(0);
    ToolAttemptOutcome::done(
        ToolOutcomeDone::ok(lash_sansio::handle::trigger_register_slot_json(
            identity.intent_index,
        )),
        ToolIntents::v3(vec![ToolIntent::RegisterTrigger(Box::new(
            lash_core::RegisterTriggerIntent {
                owner,
                owner_scope,
                actor,
                draft,
            },
        ))]),
    )
}

fn refuse(message: impl std::fmt::Display) -> ToolAttemptOutcome {
    match ToolOutcome::err_fmt(format_args!("{message}")) {
        ToolOutcome::Done(output) => {
            ToolAttemptOutcome::done_without_intents(ToolOutcomeDone::from_output(*output))
        }
        ToolOutcome::Pending(_) => unreachable!("err_fmt always produces a done outcome"),
    }
}
