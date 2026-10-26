use lash_kernel_value::identity::SessionId;
use lash_protocol_types::ExecutionScope;
use lash_trace::{TraceLanguageExecution, TraceRuntimeSubject};
use lash_vocabulary::identity::ProcessId;

/// The canonical owner of a language execution, independent of its dialect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Subject {
    Process(ProcessId),
    Session(SessionId),
}

pub(super) fn subject(execution: &TraceLanguageExecution) -> Option<Subject> {
    match &execution.subject() {
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
