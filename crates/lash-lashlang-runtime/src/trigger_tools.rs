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
    AttemptContext, SessionId, ToolAttemptOutcome, ToolCall, ToolDefinition, ToolIntent,
    ToolIntents, ToolOutcome, ToolOutcomeDone,
};
use lash_tool_support::{StaticToolExecute, StaticToolProvider, ToolDefinitionBindingExt};

use crate::trigger_commands::prepare_trigger_draft;

/// The `triggers.register` tool definition: the record the retired resource
/// operation accepted, answered by the trigger handle its realization mints.
pub fn register_trigger_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        lashlang::REGISTER_TRIGGER_TOOL_ID,
        "register_trigger",
        "Register a durable trigger subscription: the source emits occurrences \
         that start the target process with the declared inputs.",
        lashlang::register_trigger_tool_input_schema(),
        lashlang::register_trigger_tool_output_schema(),
    )
    .with_examples(vec![
        r#"await triggers.register({ source: timer.Schedule({ expr: "0 8 * * *" }), target: scan, inputs: { tick: trigger.event } })"#
            .into(),
    ])
    .with_tool_binding(lash_core::ToolBinding::new(["triggers"], "register"))
}

/// The provider a plugin registers to put `register_trigger` on the tool
/// surface. The artifact store is the module store the draft preparation
/// resolves the target's authoritative signature against.
pub fn register_trigger_tool_provider(
    artifact_store: lashlang::LashlangArtifacts,
) -> StaticToolProvider<RegisterTriggerTools> {
    StaticToolProvider::new(
        vec![register_trigger_tool_definition()],
        RegisterTriggerTools { artifact_store },
    )
}

pub struct RegisterTriggerTools {
    artifact_store: lashlang::LashlangArtifacts,
}

#[async_trait::async_trait]
impl StaticToolExecute for RegisterTriggerTools {
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        execute_register_trigger_tool_call(call.context, call.args, &self.artifact_store).await
    }
}

pub async fn execute_register_trigger_tool_call(
    context: &AttemptContext<'_>,
    args: &Value,
    artifact_store: &lashlang::LashlangArtifacts,
) -> ToolAttemptOutcome {
    let request = match lashlang::TriggerRegistrationRequest::decode(args) {
        Ok(request) => request,
        Err(error) => return refuse(error.to_string()),
    };
    let prepared = match prepare_trigger_draft(artifact_store, &request).await {
        Ok(prepared) => prepared,
        Err(error) => return refuse(error.to_string()),
    };
    let session_id = SessionId::from(context.session_id());
    let session_scope = lash_core::SessionScope::for_agent_frame(
        session_id.clone(),
        context.agent_frame_id().clone(),
    );
    // The subscription belongs to the authority that owns the caller: a
    // registration declared inside a durable process carries the chain's
    // originator and its wake target, and a session's own declaration carries
    // the session's. Same ruling the host-operation path applied (FIG-3116).
    let provenance = context.process_spawn_provenance().cloned();
    let actor = provenance
        .as_ref()
        .map(|spawn| spawn.originator.clone())
        .unwrap_or_else(|| lash_core::ProcessOriginator::session(session_scope.clone()));
    let owner_scope = match lash_core::resolve_trigger_owner_scope(
        &session_id,
        provenance.as_ref().map(|spawn| &spawn.originator),
    ) {
        Ok(scope) => scope,
        Err(error) => return refuse(error.to_string()),
    };
    let wake_target = provenance
        .as_ref()
        .and_then(|spawn| spawn.wake_session_id.as_ref())
        .map(|session| lash_core::SessionScope::new(session.clone()))
        .or(Some(session_scope));
    // A process's env is already durable, so the draft names its reference
    // verbatim and the intent publishes nothing. A session's env is not, so
    // the draft names the content-addressed reference and the intent carries
    // the spec for realization to publish under its own artifact owner.
    let (env_ref, env_spec) = match context.inherited_process_execution_env_ref() {
        Some(env_ref) => (env_ref, None),
        None => {
            let env_spec = context.process_execution_env_spec();
            match env_spec.stable_ref() {
                Ok(env_ref) => (env_ref, Some(env_spec)),
                Err(error) => {
                    return refuse(format!("failed to encode process execution env: {error}"));
                }
            }
        }
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
                session_id,
                owner_scope,
                actor,
                env_spec,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_trigger_tool_definition_exposes_the_registration_contract() {
        let definition = register_trigger_tool_definition();
        assert_eq!(
            definition.manifest.id.as_str(),
            lashlang::REGISTER_TRIGGER_TOOL_ID
        );
        assert_eq!(definition.name(), "register_trigger");

        assert_eq!(
            definition
                .manifest
                .bindings
                .get(lash_tool_support::TYPESCRIPT_TOOL_BINDING_KEY),
            Some(
                &serde_json::to_value(lash_core::ToolBinding::new(["triggers"], "register"))
                    .expect("binding serializes")
            ),
        );

        let input = &definition.contract.input_schema.canonical;
        assert_eq!(
            input["required"],
            serde_json::json!(["source", "target", "inputs"])
        );
        assert_eq!(input["additionalProperties"], serde_json::json!(false));
        assert_eq!(
            input["properties"]["target"]["x-lash"],
            serde_json::json!({ "kind": "process_unknown" })
        );

        let output = &definition.contract.output_schema.canonical;
        assert_eq!(
            output["x-lash"],
            serde_json::json!({ "kind": "handle", "payload": {} })
        );
    }

    #[tokio::test]
    async fn register_trigger_provider_advertises_only_the_registration_tool() {
        use lash_core::ToolProvider as _;

        let provider =
            register_trigger_tool_provider(crate::lib_tests::memory_artifact_store().await);
        let manifests = provider.tool_manifests();
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].name, "register_trigger");
        assert_eq!(manifests[0].id.as_str(), lashlang::REGISTER_TRIGGER_TOOL_ID);
    }
}
