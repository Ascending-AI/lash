use super::*;

use lash_core::ToolCall;
use lash_tool_support::StaticToolExecute;

use crate::SessionProcessAdminTools;

fn tools() -> SessionProcessAdminTools {
    SessionProcessAdminTools {
        include_cancel_process: true,
    }
}

fn process_handle(process_id: &str, incarnation: u64) -> Value {
    serde_json::json!({
        "__handle__": "process",
        "id": process_id,
        "incarnation": incarnation,
    })
}

fn intents(outcome: ToolAttemptOutcome) -> (Value, Vec<ToolIntent>) {
    let ToolAttemptOutcome::Done { result, intents } = outcome else {
        panic!("expected a done attempt");
    };
    let output = match result.into_output().outcome {
        lash_core::ToolCallOutcome::Success(value) => value.to_json_value(),
        other => panic!("expected a successful declaration, got {other:?}"),
    };
    (output, intents.intents)
}

fn refusal(outcome: ToolAttemptOutcome) -> String {
    let ToolAttemptOutcome::Done { result, intents } = outcome else {
        panic!("expected a done attempt");
    };
    assert!(
        intents.is_empty(),
        "a refused declaration must declare nothing"
    );
    format!("{result:?}")
}

/// The context a declaring leaf body actually receives: the attempt
/// coordinator has prepared a call id, which is what the declaration identity
/// is derived from, and names the process the body runs inside when there is
/// one.
fn attempt_context(runtime_process_id: Option<&str>) -> lash_core::ToolContext<'static> {
    lash_core::testing::mock_tool_context().__with_attempt_binding_for_testing(
        Some("declaration-call".to_string()),
        runtime_process_id.map(|id| lash_core::ProcessId::from(id.to_string())),
    )
}

macro_rules! attempt {
    ($tool:literal, $args:expr) => {
        attempt!($tool, $args, None)
    };
    ($tool:literal, $args:expr, $process:expr) => {{
        let tool_context = attempt_context($process);
        let context = lash_core::AttemptContext::__for_testing(&tool_context, "declaration-scope");
        tools()
            .execute_attempt(ToolCall {
                name: $tool,
                args: &$args,
                context: &context,
            })
            .await
    }};
}

#[tokio::test]
async fn start_process_declares_a_start_and_answers_with_the_derived_id() {
    let outcome = attempt!(
        "start_process",
        serde_json::json!({
            "definition": { "$lash_process": true, "process_name": "on_button" },
            "args": { "request": { "id": "req-1" } },
        })
    );
    let (output, declared) = intents(outcome);
    let [ToolIntent::StartProcess(intent)] = declared.as_slice() else {
        panic!("expected exactly one start declaration, got {declared:?}");
    };
    let lash_core::ProcessInput::Engine { kind, payload } = &intent.declaration.input else {
        panic!("a start declares an engine input");
    };
    assert_eq!(kind, DEFAULT_PROCESS_ENGINE_KIND);
    // The engine payload is the definition value plus this run's arguments:
    // nothing in the plugin reshapes the engine's own encoding.
    assert_eq!(
        payload.get("process_name"),
        Some(&serde_json::json!("on_button"))
    );
    assert_eq!(
        payload.get("args"),
        Some(&serde_json::json!({ "request": { "id": "req-1" } }))
    );

    // The id the attempt answers with is the id the declaration's own identity
    // derives, so the executor starts that same id on the first run and on
    // every redrive of this attempt.
    let identity = {
        let tool_context = attempt_context(None);
        let context = lash_core::AttemptContext::__for_testing(&tool_context, "declaration-scope");
        context.intent_identity(0).expect("a derivable identity")
    };
    let expected = lash_core::ProcessId::from_intent_identity(&identity);
    assert_eq!(output.get("id"), Some(&serde_json::json!(expected)));
    assert_eq!(output.get("process_id"), Some(&serde_json::json!(expected)));
}

#[tokio::test]
async fn start_process_takes_the_engine_kind_a_third_party_names() {
    let outcome = attempt!(
        "start_process",
        serde_json::json!({
            "definition": { "definition_id": "scheduler-job" },
            "engine": "third-party-engine",
        })
    );
    let (_, declared) = intents(outcome);
    let [ToolIntent::StartProcess(intent)] = declared.as_slice() else {
        panic!("expected one start declaration");
    };
    let lash_core::ProcessInput::Engine { kind, payload } = &intent.declaration.input else {
        panic!("a start declares an engine input");
    };
    assert_eq!(kind, "third-party-engine");
    assert_eq!(payload.get("args"), Some(&serde_json::json!({})));
}

#[tokio::test]
async fn start_process_refuses_a_definition_that_is_not_a_process_value() {
    let message = refusal(attempt!(
        "start_process",
        serde_json::json!({ "definition": "on_button" })
    ));
    assert!(message.contains("process definition value"), "{message}");
}

#[tokio::test]
async fn signal_process_declares_the_signal_the_handle_names() {
    let outcome = attempt!(
        "signal_process",
        serde_json::json!({
            "handle": process_handle("process-7", 3),
            "name": "approved",
            "payload": { "by": "sam" },
        })
    );
    let (_, declared) = intents(outcome);
    let [ToolIntent::SignalProcess(intent)] = declared.as_slice() else {
        panic!("expected one signal declaration, got {declared:?}");
    };
    assert_eq!(intent.process_id.as_str(), "process-7");
    assert_eq!(intent.signal_name, "approved");
    assert_eq!(intent.payload, serde_json::json!({ "by": "sam" }));
}

#[tokio::test]
async fn signal_process_refuses_a_value_that_is_not_a_process_handle() {
    let message = refusal(attempt!(
        "signal_process",
        serde_json::json!({ "handle": { "id": "process-7" }, "name": "approved" })
    ));
    assert!(message.contains("Invalid process handle"), "{message}");
}

#[tokio::test]
async fn emit_process_event_is_refused_outside_a_process() {
    // A cell has no enclosing process, and the event this tool appends belongs
    // to the process the call runs inside. Refusing is the contract, so it says
    // so rather than appending nowhere.
    let message = refusal(attempt!(
        "emit_process_event",
        serde_json::json!({ "value": { "stage": "approved" } })
    ));
    assert!(
        message.contains("running inside a durable process"),
        "{message}"
    );
}

#[tokio::test]
async fn emit_process_event_declares_an_append_to_its_own_process() {
    let outcome = attempt!(
        "emit_process_event",
        serde_json::json!({ "value": { "stage": "approved" } }),
        Some("process-9")
    );
    let (_, declared) = intents(outcome);
    let [ToolIntent::EmitProcessEvent(intent)] = declared.as_slice() else {
        panic!("expected one append declaration, got {declared:?}");
    };
    assert_eq!(intent.process_id.as_str(), "process-9");
    assert_eq!(intent.payload, serde_json::json!({ "stage": "approved" }));
}

#[tokio::test]
async fn register_process_declares_a_registration_it_cannot_yet_realize() {
    let outcome = attempt!(
        "register_process",
        serde_json::json!({
            "name": "approval",
            "definition": { "$lash_process": true, "process_name": "on_button" },
        })
    );
    let (output, declared) = intents(outcome);
    let [ToolIntent::RegisterProcessDefinition(intent)] = declared.as_slice() else {
        panic!("expected one registration declaration, got {declared:?}");
    };
    assert_eq!(intent.engine_kind, DEFAULT_PROCESS_ENGINE_KIND);
    assert_eq!(intent.label.as_deref(), Some("approval"));
    assert_eq!(output.get("name"), Some(&serde_json::json!("approval")));
}

#[tokio::test]
async fn register_process_refuses_an_empty_name() {
    let message = refusal(attempt!(
        "register_process",
        serde_json::json!({ "name": "   ", "definition": {} })
    ));
    assert!(message.contains("non-empty `name`"), "{message}");
}

#[test]
fn declaring_tools_type_their_process_arguments_as_processes() {
    for definition in [
        process_start_tool_definition(),
        process_register_tool_definition(),
    ] {
        let schema = definition.contract().input_schema.canonical().clone();
        let declared = schema
            .get("properties")
            .and_then(|properties| properties.get("definition"))
            .and_then(|property| property.get("x-lash"))
            .cloned();
        assert_eq!(
            declared,
            Some(serde_json::json!({ "kind": "process_unknown" })),
            "{} did not type its definition argument as a process",
            definition.name()
        );
    }
}
