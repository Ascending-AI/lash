//! The shared refusal corpus: one registration per core validation rule.
//!
//! Core validation and remote ingress are two validators over one contract
//! (FIG-2985). This corpus is the single place a refused shape is spelled, and
//! both sides consume it: the core parity test asserts every fixture trips the
//! rule it was written for, and `lash-remote-protocol`'s decoder parity test
//! pushes the same fixtures through the peer-facing decoder and asserts a typed
//! refusal rather than a panic.
//!
//! [`refused_process_registrations`] matches [`ProcessRegistrationRefusal`]
//! exhaustively, so a new core rule cannot be added without a fixture, and the
//! fixtures are automatically fed to the remote decoder. A rule reached from
//! more than one input arm carries one fixture per arm, because an arm nobody
//! spells is an arm neither validator is proved to refuse.

use super::super::events::{ProcessEventSemanticsSpec, ProcessEventType, ProcessTerminalSpec};
use super::super::model::{
    OnParentEnd, ParentScope, ProcessExecutionEnvRef, ProcessInput, ProcessLifecyclePolicy,
    ProcessProvenance, ProcessRegistration, ProcessStatus, RecoveryContract,
};
use super::super::validation::ProcessRegistrationRefusal;

/// A syntactically valid execution-env ref; the digest is fixed, never measured.
const FIXTURE_ENV_REF: &str = concat!(
    "process-env:v6:blake3:",
    "0000000000000000000000000000000000000000000000000000000000000000"
);

/// The process id every fixture uses, so refusal messages are comparable.
pub const REFUSAL_FIXTURE_PROCESS_ID: &str = "refusal-fixture";

fn host_registration(input: ProcessInput) -> ProcessRegistration {
    ProcessRegistration::new(
        REFUSAL_FIXTURE_PROCESS_ID,
        input,
        RecoveryContract::ExternallyOwned,
        ProcessProvenance::host(),
        ProcessLifecyclePolicy::new(ParentScope::Host, OnParentEnd::Abandon),
    )
}

/// A registration core accepts, and the base every fixture below mutates.
pub fn accepted_process_registration() -> ProcessRegistration {
    host_registration(ProcessInput::External {
        metadata: serde_json::Value::Null,
    })
}

fn tool_call_input(call_id: &str, tool_name: &str) -> ProcessInput {
    ProcessInput::ToolCall {
        call: crate::PreparedToolCall::from_parts(
            call_id,
            crate::ToolId::new("fixture-tool-id"),
            tool_name,
            serde_json::json!({}),
            None,
            serde_json::Value::Null,
        ),
    }
}

fn env_ref() -> Option<ProcessExecutionEnvRef> {
    Some(ProcessExecutionEnvRef::new(FIXTURE_ENV_REF.to_string()))
}

fn with_event_type(event_type: ProcessEventType) -> ProcessRegistration {
    let mut registration = accepted_process_registration();
    registration.event_types.push(event_type);
    registration
}

fn custom_event_type(name: &str, semantics: ProcessEventSemanticsSpec) -> ProcessEventType {
    ProcessEventType {
        name: name.to_string(),
        payload_schema: crate::LashSchema::any(),
        semantics,
    }
}

/// A session-turn input with an otherwise valid definition key.
fn session_turn_input(definition_key: &str) -> ProcessInput {
    ProcessInput::SessionTurn {
        definition_key: definition_key.to_string(),
        create_request: Box::new(
            crate::SessionCreateRequest::root(
                crate::SessionStartPoint::Empty,
                crate::PluginOptions::default(),
            )
            .with_session_id("refusal-fixture-child"),
        ),
        turn_input: Box::new(crate::TurnInput::empty()),
        output_contract: crate::ToolOutputContract::Static,
    }
}

/// The match is exhaustive on purpose: adding a rule to
/// [`ProcessRegistrationRefusal`] stops this function compiling until the shape
/// is spelled here, and both validators then see it.
pub fn refused_process_registrations(rule: ProcessRegistrationRefusal) -> Vec<ProcessRegistration> {
    match rule {
        ProcessRegistrationRefusal::HostParentCancels => {
            let mut registration = accepted_process_registration();
            registration.lifecycle =
                ProcessLifecyclePolicy::new(ParentScope::Host, OnParentEnd::Cancel);
            vec![registration]
        }
        ProcessRegistrationRefusal::TurnParentSessionMismatch => {
            let mut registration = accepted_process_registration();
            registration.lifecycle = ProcessLifecyclePolicy::new(
                ParentScope::turn("parent-session", "turn-1"),
                OnParentEnd::Abandon,
            );
            registration.provenance = ProcessProvenance::session(crate::SessionScope::new(
                "a-different-session".to_string(),
            ));
            vec![registration]
        }
        ProcessRegistrationRefusal::InvalidProcessKey => {
            let mut registration = accepted_process_registration();
            registration.id = "refusal#fixture".into();
            vec![registration]
        }
        ProcessRegistrationRefusal::ZeroMaxAttempts => {
            let mut registration = accepted_process_registration();
            registration.max_attempts = Some(0);
            vec![registration]
        }
        ProcessRegistrationRefusal::ToolCallWithoutCallId => {
            let mut registration = host_registration(tool_call_input("   ", "fixture-tool"));
            registration.env_ref = env_ref();
            vec![registration]
        }
        ProcessRegistrationRefusal::ToolCallWithoutToolName => {
            let mut registration = host_registration(tool_call_input("fixture-call", "\t"));
            registration.env_ref = env_ref();
            vec![registration]
        }
        // Both arms that must carry an env: engine and tool-call.
        ProcessRegistrationRefusal::ExecutionEnvMissing => vec![
            host_registration(ProcessInput::Engine {
                kind: "fixture-engine".to_string(),
                payload: serde_json::Value::Null,
            }),
            host_registration(tool_call_input("fixture-call", "fixture-tool")),
        ],
        // Both arms that must not: external and session-turn.
        ProcessRegistrationRefusal::ExecutionEnvNotAllowed => {
            let mut external = accepted_process_registration();
            external.env_ref = env_ref();
            let mut session_turn = host_registration(session_turn_input("fixture-definition"));
            session_turn.env_ref = env_ref();
            vec![external, session_turn]
        }
        ProcessRegistrationRefusal::EmptySessionTurnDefinitionKey => {
            vec![host_registration(session_turn_input("  "))]
        }
        ProcessRegistrationRefusal::EmptyEventTypeName => vec![with_event_type(custom_event_type(
            "  ",
            ProcessEventSemanticsSpec::default(),
        ))],
        ProcessRegistrationRefusal::DuplicateEventType => {
            let mut registration = with_event_type(custom_event_type(
                "app.duplicated",
                ProcessEventSemanticsSpec::default(),
            ));
            registration.event_types.push(custom_event_type(
                "app.duplicated",
                ProcessEventSemanticsSpec::default(),
            ));
            vec![registration]
        }
        ProcessRegistrationRefusal::ReservedRuntimeEventType => {
            // Redeclares a reserved name the defaults already carry, with a
            // schema the runtime does not own; pushing a second copy would trip
            // the duplicate rule first.
            let mut registration = accepted_process_registration();
            let declared = registration
                .event_types
                .iter_mut()
                .find(|event_type| event_type.name == "process.waiting")
                .expect("the default event types declare `process.waiting`");
            declared.payload_schema = crate::LashSchema::new(serde_json::json!({"type": "object"}));
            vec![registration]
        }
        ProcessRegistrationRefusal::NonTerminalTerminalStatus => {
            vec![with_event_type(custom_event_type(
                "app.terminal",
                ProcessEventSemanticsSpec {
                    terminal: Some(ProcessTerminalSpec {
                        status: ProcessStatus::Running,
                        await_output: Some(crate::ProcessValueSelector::Pointer(
                            "/await_output".to_string(),
                        )),
                    }),
                    ..ProcessEventSemanticsSpec::default()
                },
            ))]
        }
        ProcessRegistrationRefusal::TerminalEventWithoutAwaitOutput => {
            vec![with_event_type(custom_event_type(
                "app.terminal",
                ProcessEventSemanticsSpec {
                    terminal: Some(ProcessTerminalSpec {
                        status: ProcessStatus::Failed,
                        await_output: None,
                    }),
                    ..ProcessEventSemanticsSpec::default()
                },
            ))]
        }
    }
}
