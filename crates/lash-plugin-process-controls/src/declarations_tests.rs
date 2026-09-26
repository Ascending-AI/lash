use super::*;

use lash_core::ToolCall;
use lash_tool_support::StaticToolExecute;

use crate::SessionProcessAdminTools;

fn tools() -> SessionProcessAdminTools {
    SessionProcessAdminTools {
        include_cancel_process: true,
    }
}

fn process_handle(process_id: &lash_core::ProcessId) -> Value {
    lash_core::RuntimeExecutionContext::process_handle_json(process_id)
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
fn attempt_context(enclosing_process: Option<&str>) -> lash_core::ToolContext<'static> {
    // A recorded attempt always runs under a real owner scope; the mock
    // default is `RuntimeOperation`, which names no opener.
    let scoped = lash_core::ScopedEffectController::shared(
        std::sync::Arc::new(lash_core::testing::UnavailableEffectController),
        lash_core::AdmittedScope::turn("test-session", "declaration-turn"),
    )
    .expect("the test scope validates");
    lash_core::testing::mock_tool_context()
        .__with_scoped_effect_controller_for_testing(scoped)
        .__with_attempt_binding_for_testing(
            Some("declaration-call".to_string()),
            enclosing_process.map(lash_core::ProcessId::fixture),
        )
}

macro_rules! attempt {
    ($tool:literal, $args:expr) => {
        attempt!($tool, $args, None)
    };
    ($tool:literal, $args:expr, $process:expr) => {{
        let tool_context = attempt_context($process);
        let context = lash_core::AttemptContext::__for_testing(&tool_context, "declaration-scope");
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

#[tokio::test]
async fn start_process_declares_a_start_and_answers_with_its_start_slot() {
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

    // The answer is a start slot, never a handle (ADR 0107): a declaring
    // attempt cannot know the id the registrar will mint, so it names its own
    // intent index, and the attempt coordinator replaces the slot with the
    // realized start's handle before any model or cell sees the output. The
    // slot is not a handle record, so an unrealized start can never be
    // mistaken for a started process.
    assert_eq!(output, lash_sansio::handle::process_start_slot_json(0));
    assert!(
        output.get(lash_sansio::handle::HANDLE_FIELD).is_none(),
        "the unrealized answer carries no handle"
    );
}

/// A start outside any chain is a session start, so the calling session is the
/// wake target of everything the started process declares. Without it the
/// process runs, its `processes.emit` materializes a wake, and the wake is
/// dropped for want of a delivery target — the session waiting on it never
/// sees queued work.
#[tokio::test]
async fn a_session_start_declares_the_calling_session_as_its_wake_target() {
    let outcome = attempt!(
        "start_process",
        serde_json::json!({
            "definition": { "$lash_process": true, "process_name": "waker" },
        })
    );
    let (_, declared) = intents(outcome);
    let [ToolIntent::StartProcess(intent)] = declared.as_slice() else {
        panic!("expected one start declaration, got {declared:?}");
    };
    let session_id = {
        let tool_context = attempt_context(None);
        lash_core::SessionId::from(tool_context.session_id())
    };
    assert_eq!(
        intent.declaration.wake_session_id.as_ref(),
        Some(&session_id),
        "a session start must name its own session as the wake target"
    );
    let lash_core::ProcessOriginator::Session {
        session_id: originator_session_id,
        ..
    } = &intent.declaration.originator
    else {
        panic!("a session start is originated by its session");
    };
    assert_eq!(
        originator_session_id, &session_id,
        "originator and wake target are the same session for a session start"
    );
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
            "handle": process_handle(&lash_core::ProcessId::fixture("process-7")),
            "name": "approved",
            "payload": { "by": "sam" },
        })
    );
    let (_, declared) = intents(outcome);
    let [ToolIntent::SignalProcess(intent)] = declared.as_slice() else {
        panic!("expected one signal declaration, got {declared:?}");
    };
    assert_eq!(
        intent.process_id,
        lash_core::ProcessId::fixture("process-7")
    );
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
    assert_eq!(
        intent.process_id,
        lash_core::ProcessId::fixture("process-9")
    );
    assert_eq!(intent.payload, serde_json::json!({ "stage": "approved" }));
}

#[tokio::test]
async fn register_process_declares_a_registration_that_claims_the_name() {
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
    assert_eq!(intent.name.as_deref(), Some("approval"));
    assert_eq!(intent.expected_revision, None);
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

/// The definition a caller starts is the definition `processes.list` filters
/// by. The tool puts the definition value verbatim in the engine payload, the
/// engine's own admission decodes that payload back into the definition
/// reference it stores on the row, and the same value passed as
/// `processes.list({ definition })` selects that row. Nothing in this path is
/// the plugin's own encoding: the value is minted and read by the engine, and
/// the plugin only carries it.
#[tokio::test]
async fn a_started_definition_is_the_definition_processes_list_filters_by() {
    let component = lashlang::ContentHash::new(
        "0000000000000000000000000000000000000000000000000000000000000001",
    );
    let definition = lashlang::ProcessDefinitionIdentity::new(
        lashlang::ModuleRef::new(&component),
        lashlang::HostRequirementsRef::new(&component),
        lashlang::ProcessRef::new(component.clone(), 0),
        "review",
    )
    .to_process_value();

    let (_, declared) = intents(attempt!(
        "start_process",
        serde_json::json!({ "definition": definition, "args": { "topic": "handles" } })
    ));
    let [ToolIntent::StartProcess(start)] = declared.as_slice() else {
        panic!("expected one start declaration, got {declared:?}");
    };
    let lash_core::ProcessInput::Engine { kind, payload } = &start.declaration.input else {
        panic!("a definition start is an engine start");
    };

    // The owning engine admits its own payload and names the definition itself.
    let identity = lash_lashlang_runtime::admit_lashlang_process(
        lash_lashlang_runtime::LASHLANG_ENGINE_KIND,
        payload,
        None,
    )
    .expect("the lashlang engine admits the payload the tool declared");
    assert_eq!(kind, lash_lashlang_runtime::LASHLANG_ENGINE_KIND);

    let mut registration = lash_core::ProcessRegistration::new(
        start.declaration.input.clone(),
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::host(),
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Host,
            lash_core::OnParentEnd::Abandon,
        ),
    )
    .with_execution_env_ref(Some(lash_core::ProcessExecutionEnvRef::new(
        "process-env:fig-3000-declaration-test",
    )));
    registration.identity = identity;
    let record = lash_core::ProcessRecord::from_registration(
        registration,
        lash_core::ProcessId::fixture("process-review"),
    );

    let filter = lash_core::ProcessListFilter::decode(&serde_json::json!({
        "definition": definition,
        "status": "any",
    }))
    .expect("the definition value is a valid list filter");
    assert!(
        filter.matches_record(&record),
        "the process started from a definition must be the one listing by it returns"
    );

    let other = lashlang::ProcessDefinitionIdentity::new(
        lashlang::ModuleRef::new(&component),
        lashlang::HostRequirementsRef::new(&component),
        lashlang::ProcessRef::new(component, 1),
        "summarize",
    )
    .to_process_value();
    let other_filter = lash_core::ProcessListFilter::decode(&serde_json::json!({
        "definition": other,
        "status": "any",
    }))
    .expect("the definition value is a valid list filter");
    assert!(
        !other_filter.matches_record(&record),
        "a different definition must not select this process"
    );
}

/// FIG-3122 law (b)/(c) at the declaring seam: two starts of the same
/// definition with different labels carry the same engine payload — the same
/// `module_ref`, `process_ref` and `process_name` a lifted literal answers —
/// and differ only in the declared label. A label is host-facing display
/// metadata; it is never an input to what the row identifies.
#[tokio::test]
async fn two_starts_of_one_definition_differ_only_in_the_declared_label() {
    let definition = serde_json::json!({
        "$lash_process": true,
        "module_ref": "lashlang:v2:blake3:93b4cbf8fa9ac47be98eaec61083ef95dda7df8c6efaf028b81468351c203523",
        "process_ref": { "component": "57a0dc64da4566efeae196f9607a0278bfbb7913ab9ac9dd44e90d452703d69a", "pos": 0 },
        "process_name": "__process_02178275819fb79b903c9a8b03a8b2d28c41708383b1728900e429e3a59b6a32",
    });
    let start = |label: &str| {
        let definition = definition.clone();
        let args = serde_json::json!({ "definition": definition, "label": label });
        async move {
            let (_, declared) = intents(attempt!("start_process", args));
            let [ToolIntent::StartProcess(intent)] = declared.as_slice() else {
                panic!("expected exactly one start declaration, got {declared:?}");
            };
            intent.declaration.clone()
        }
    };

    let first = start("probe-a").await;
    let second = start("probe-b").await;

    let lash_core::ProcessInput::Engine {
        payload: first_payload,
        ..
    } = &first.input
    else {
        panic!("a start declares an engine input");
    };
    let lash_core::ProcessInput::Engine {
        payload: second_payload,
        ..
    } = &second.input
    else {
        panic!("a start declares an engine input");
    };
    assert_eq!(
        first_payload, second_payload,
        "the label is not part of the engine payload, so the definition bytes are identical"
    );
    assert_eq!(
        first_payload.get("module_ref"),
        Some(&definition["module_ref"]),
        "the module ref the literal lifted to is untouched"
    );
    assert_eq!(
        first
            .identity
            .as_ref()
            .and_then(|identity| identity.label.as_deref()),
        Some("probe-a")
    );
    assert_eq!(
        second
            .identity
            .as_ref()
            .and_then(|identity| identity.label.as_deref()),
        Some("probe-b")
    );
}

/// A start that names no label declares none, so the engine's derived label —
/// for Lashlang, the lift digest — stays the row's label.
#[tokio::test]
async fn a_start_without_a_label_declares_no_identity() {
    let outcome = attempt!(
        "start_process",
        serde_json::json!({ "definition": { "$lash_process": true, "process_name": "on_button" } })
    );
    let (_, declared) = intents(outcome);
    let [ToolIntent::StartProcess(intent)] = declared.as_slice() else {
        panic!("expected one start declaration");
    };
    assert!(intent.declaration.identity.is_none());
}

/// The argument is documented, so an unusable value is refused rather than
/// dropped: silently ignoring it is the defect this plumbing fixes.
#[tokio::test]
async fn start_process_refuses_a_label_that_is_not_a_usable_string() {
    for label in [serde_json::json!(7), serde_json::json!("   ")] {
        let message = refusal(attempt!(
            "start_process",
            serde_json::json!({
                "definition": { "$lash_process": true, "process_name": "on_button" },
                "label": label,
            })
        ));
        assert!(message.contains("`label`"), "{message}");
    }
}
