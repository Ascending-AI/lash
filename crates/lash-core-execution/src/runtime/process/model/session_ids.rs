use super::{ProcessId, SessionId};

/// The child session a `ProcessInput::SessionTurn` process runs its turn in
/// when its create request names none: derived from the minted process id, so
/// every redrive of the process reopens the same session and no two processes
/// ever share one (ADR 0107).
pub fn process_child_session_id(process_id: &ProcessId) -> SessionId {
    SessionId::from(format!("session:process:{process_id}"))
}

/// Durable session stores owned exclusively by one process execution.
pub fn process_runtime_session_ids(process_id: &ProcessId) -> [SessionId; 2] {
    [
        SessionId::from(format!("process-env:{process_id}")),
        SessionId::from(format!("process-session-turn:{process_id}")),
    ]
}
