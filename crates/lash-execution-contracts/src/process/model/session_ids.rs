use lash_kernel_value::identity::SessionId;
use lash_vocabulary::identity::ProcessId;

/// The child session a `ProcessInput::SessionTurn` process runs its turn in
/// when its create request names none: derived from the minted process id, so
/// every redrive of the process reopens the same session and no two processes
/// ever share one (ADR 0107).
pub fn process_child_session_id(process_id: &ProcessId) -> SessionId {
    SessionId::prefixed("session:process:", process_id)
}

/// The turn a `ProcessInput::SessionTurn` process runs in its child session:
/// named by the process id, so the child run names the process whose own
/// run executes it. The process executes that run inline, in its own
/// execution; it never runs as a run of its own (FIG-4378).
pub fn process_session_turn_id(process_id: &ProcessId) -> lash_vocabulary::identity::TurnId {
    lash_vocabulary::identity::TurnId::from(process_id)
}

/// The session a session-turn process starts: the stated id, or its stable child id.
pub fn process_session_id(process: &ProcessId, request: &crate::SessionCreateRequest) -> SessionId {
    request
        .session_id
        .clone()
        .unwrap_or_else(|| process_child_session_id(process))
}
