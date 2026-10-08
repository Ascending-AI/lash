//! The shared refusal corpus: one registration per core validation rule.
//!
//! Core validation and remote ingress are two validators over one contract
//! (FIG-2985). This corpus is the single place a refused shape is spelled, and
//! both sides consume it: the core parity test asserts every fixture trips the
//! rule it was written for, and the core admission law
//! pushes the same fixtures through the peer-facing decoder and asserts a typed
//! refusal rather than a panic.
//!
//! [`refused_process_registrations`] matches [`ProcessRegistrationRefusal`]
//! exhaustively, so a new core rule cannot be added without a fixture, and the
//! fixtures are automatically fed to the remote decoder. A rule reached from
//! more than one input arm carries one fixture per arm, because an arm nobody
//! spells is an arm neither validator is proved to refuse.

use super::super::model::{
    Ancestry, Lifetime, LifetimeDecision, ProcessExecutionEnvRef, ProcessInput, ProcessProvenance,
    ProcessRegistration, ScopeGrant, ScopeId,
};
use super::super::validation::ProcessRegistrationRefusal;

/// A syntactically valid execution-env ref; the digest is fixed, never measured.
const FIXTURE_ENV_REF: &str = concat!(
    "process-env:v6:blake3:",
    "0000000000000000000000000000000000000000000000000000000000000000"
);

/// The host start key every fixture uses, so refusal messages are comparable.
pub const REFUSAL_FIXTURE_START_KEY: &str = "refusal-fixture";

fn host_registration(input: ProcessInput) -> ProcessRegistration {
    ProcessRegistration::new(input, ProcessProvenance::host(), Lifetime::Detached)
        .with_start_key(Some(crate::StartKey::for_host(REFUSAL_FIXTURE_START_KEY)))
}

/// A registration core accepts, and the base every fixture below mutates.
pub fn accepted_process_registration() -> ProcessRegistration {
    host_registration(ProcessInput::Engine {
        kind: "fixture-engine".to_string(),
        payload: serde_json::Value::Null,
    })
    .with_execution_env_ref(env_ref())
}

fn env_ref() -> Option<ProcessExecutionEnvRef> {
    Some(ProcessExecutionEnvRef::new(FIXTURE_ENV_REF.to_string()))
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
        result: crate::SessionTurnOutcome::Turn,
    }
}

/// The match is exhaustive on purpose: adding a rule to
/// [`ProcessRegistrationRefusal`] stops this function compiling until the shape
/// is spelled here, and both validators then see it.
pub fn refused_process_registrations(rule: ProcessRegistrationRefusal) -> Vec<ProcessRegistration> {
    match rule {
        ProcessRegistrationRefusal::LifetimeScopeUnreachable => {
            // A root start names a turn it was never admitted under.
            let mut registration = accepted_process_registration();
            registration.lifetime = LifetimeDecision::Until {
                scope: ScopeId::turn("fixture-session", "fixture-turn"),
                grant: ScopeGrant::Ancestor,
            };
            vec![registration]
        }
        ProcessRegistrationRefusal::HostGrantOutsideRoot => {
            // A host session-lookup grant on a runtime start, and on a scope
            // that is not a session.
            let mut runtime_start = accepted_process_registration();
            runtime_start.ancestry = Ancestry::root();
            runtime_start.lifetime = LifetimeDecision::Until {
                scope: ScopeId::turn("fixture-session", "fixture-turn"),
                grant: ScopeGrant::HostSessionLookup,
            };
            vec![runtime_start]
        }
        ProcessRegistrationRefusal::SessionCapabilityUnreachable => {
            // A root that claims a session its lifetime never looked up.
            let mut registration = accepted_process_registration();
            registration.lifetime = LifetimeDecision::Detached;
            registration.session_capability = Some(crate::SessionId::from("a-different-session"));
            vec![registration]
        }
        // Engine and session-turn inputs must carry an env.
        ProcessRegistrationRefusal::ExecutionEnvMissing => {
            vec![
                host_registration(ProcessInput::Engine {
                    kind: "fixture-engine".to_string(),
                    payload: serde_json::Value::Null,
                }),
                host_registration(session_turn_input("fixture-definition")),
            ]
        }
        ProcessRegistrationRefusal::EmptySessionTurnDefinitionKey => {
            vec![host_registration(session_turn_input("  "))]
        }
    }
}
