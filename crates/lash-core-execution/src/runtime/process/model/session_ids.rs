use super::{ProcessId, SessionId};

/// The child session a `ProcessInput::SessionTurn` process runs its turn in
/// when its create request names none: derived from the minted process id, so
/// every redrive of the process reopens the same session and no two processes
/// ever share one (ADR 0107).
pub fn process_child_session_id(process_id: &ProcessId) -> SessionId {
    SessionId::from(format!("session:process:{process_id}"))
}

/// The turn a `ProcessInput::SessionTurn` process runs in its child session:
/// named by the process id, so the child root names the process whose own
/// run executes it. The process drives that root inline, in its own
/// execution; it never runs as a root run of its own (FIG-4378).
pub fn process_session_turn_id(process_id: &ProcessId) -> crate::TurnId {
    crate::TurnId::from(process_id.as_str())
}
