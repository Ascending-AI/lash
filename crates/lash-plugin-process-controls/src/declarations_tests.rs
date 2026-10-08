use super::*;

use lash_core::ToolCall;
use lash_tool_support::StaticToolExecute;

use crate::SessionProcessAdminTools;

fn tools() -> SessionProcessAdminTools {
    SessionProcessAdminTools {
        include_cancel_process: true,
        lifetime: std::sync::Arc::new(lash_core::lifetime::session_or_starter),
    }
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
fn attempt_context(
    enclosing_process: Option<&str>,
) -> lash_core::testing::ToolCallFixture<'static> {
    // A recorded attempt always runs under a real owner scope; the mock
    // default is `RuntimeOperation`, which names no opener.
    let scoped = lash_core::ActorContext::unavailable()
        .scoped(lash_core::AdmittedScope::turn(
            "test-session",
            "declaration-turn",
        ))
        .expect("the test scope validates");
    lash_core::testing::ToolCallFixture::mock()
        .scoped_effect_controller(scoped)
        .call_id(lash_core::ToolCallId::fixture("declaration-call"))
        .enclosing_process_id(enclosing_process.map(lash_core::ProcessId::fixture))
}

macro_rules! attempt {
    ($tool:literal, $args:expr) => {
        attempt!($tool, $args, None)
    };
    ($tool:literal, $args:expr, $process:expr) => {{
        let context = attempt_context($process).attempt("declaration-scope");
        let tools = tools();
        let manifest = crate::processes_tool_definitions(true)
            .into_iter()
            .find(|definition| definition.name() == $tool)
            .expect("process-controls manifest resolves")
            .manifest();
        tools
            .execute(ToolCall::new(&manifest, &$args, &context))
            .await
    }};
}

fn id(byte: u8) -> lash_core::ProcessDefinitionId {
    lash_core::ProcessDefinitionId::from_sha256_digest([byte; 32])
}
fn definition() -> Value {
    serde_json::to_value(lash_core::ProcessDefinition::new(
        id(1),
        lash_core::ProcessSignature::Unknown,
    ))
    .expect("definition")
}

#[tokio::test]
async fn start_and_get_accept_only_the_exact_definition_contracts() {
    let (_, declared) = intents(attempt!(
        "start_process",
        serde_json::json!({"definition_id": id(1).to_tagged_json()})
    ));
    let [ToolIntent::StartProcess(intent)] = declared.as_slice() else {
        panic!("one start")
    };
    assert!(
        matches!(&intent.declaration.input, lash_core::ProcessStartTarget::Definition {definition_id, args, ..} if definition_id == &id(1) && args.is_empty())
    );
    let (slot, get) = intents(attempt!(
        "get_process_definition",
        serde_json::json!({"definition_id": id(1).to_tagged_json()})
    ));
    assert_eq!(slot, lash_sansio::handle::definition_slot_json(0));
    assert!(
        matches!(&get[..], [ToolIntent::GetDefinition(intent)] if intent.definition_id == id(1))
    );
    for args in [
        serde_json::json!({}),
        serde_json::json!({"definition": definition(), "definition_id": id(1).to_tagged_json()}),
        serde_json::json!({"definition_id": id(1).to_string()}),
        serde_json::json!({"definition": {"$lash_process": true, "process_name": "legacy"}}),
        serde_json::json!({"definition_id": id(1).to_tagged_json(), "args": []}),
    ] {
        refusal(attempt!("start_process", args));
    }
    for key in [
        "engine",
        "name",
        "revision",
        "replace",
        "expected_revision",
        "process_name",
    ] {
        let mut args = serde_json::json!({"definition": definition()});
        args[key] = serde_json::json!("forbidden");
        assert!(refusal(attempt!("start_process", args)).contains("unknown"));
        let mut args = serde_json::json!({"definition_id": id(1).to_tagged_json()});
        args[key] = serde_json::json!("forbidden");
        refusal(attempt!("get_process_definition", args));
    }
}

#[tokio::test]
async fn two_starts_of_one_definition_differ_only_in_the_declared_label() {
    let mut declarations = Vec::new();
    for label in ["first", "second"] {
        let (_, declared) = intents(attempt!(
            "start_process",
            serde_json::json!({"definition": definition(), "label": label})
        ));
        let [ToolIntent::StartProcess(intent)] = declared.as_slice() else {
            panic!("start")
        };
        assert_eq!(
            intent
                .declaration
                .identity
                .as_ref()
                .and_then(|i| i.label.as_deref()),
            Some(label)
        );
        declarations.push(intent.declaration.input.clone());
    }
    assert_eq!(declarations[0], declarations[1]);
    for label in [
        serde_json::Value::Null,
        serde_json::json!(7),
        serde_json::json!(" "),
    ] {
        assert!(
            refusal(attempt!(
                "start_process",
                serde_json::json!({"definition": definition(), "label": label})
            ))
            .contains("label")
        );
    }
}

#[test]
fn a_started_definition_is_the_definition_processes_list_filters_by() {
    let registration = lash_core::testing::held_engine_registration(
        Value::Null,
        lash_core::ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    );
    let mut record = lash_core::ProcessRecord::from_registration(
        registration,
        lash_core::ProcessId::fixture("run"),
    );
    record.identity.definition_id = Some(id(1));
    for (candidate, matches) in [(id(1), true), (id(2), false)] {
        let filter = lash_core::ProcessListFilter::decode(
            &serde_json::json!({"definition_id": candidate.to_tagged_json(), "status": "any"}),
        )
        .expect("id filter");
        assert_eq!(filter.matches_record(&record), matches);
    }
}
