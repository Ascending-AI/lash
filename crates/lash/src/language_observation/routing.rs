use lash_sansio::{ExecutionScope, ProcessId, SessionId};
use lash_trace::{TraceLanguageExecution, TraceRuntimeSubject};

/// The canonical owner of a language execution, independent of its dialect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Subject {
    Process(ProcessId),
    Session(SessionId),
}

pub(super) fn subject(execution: &TraceLanguageExecution) -> Option<Subject> {
    match &execution.identity.subject {
        TraceRuntimeSubject::Process { process_id } => Some(Subject::Process(process_id.clone())),
        TraceRuntimeSubject::Effect { address, .. } => match &address.execution_scope {
            ExecutionScope::Turn { session_id, .. }
            | ExecutionScope::SessionOperation { session_id, .. }
            | ExecutionScope::SessionDelete { session_id } => {
                Some(Subject::Session(session_id.clone()))
            }
            ExecutionScope::Process { process_id } => Some(Subject::Process(process_id.clone())),
            ExecutionScope::RuntimeOperation { .. } => None,
        },
    }
}
