use super::*;
use crate::durable_wait::restate_await_event_key;

#[test]
fn session_administrative_read_rejects_non_session_scope_aliases() {
    for scope in [
        ExecutionScope::process(lash_core::ProcessId::fixture("alias-process")),
        ExecutionScope::runtime_operation("alias-operation"),
    ] {
        let alias = SessionId::fixture(durable_wait_index_key_for_scope(&scope));
        let key = restate_await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion(lash_core::ToolCallId::fixture("alias-wait")),
        )
        .expect("derive non-session wait key");

        assert!(
            outstanding_owned_by_session(&alias, vec![key]).is_empty(),
            "a non-session wait indexed at `{alias}` must not be advertised as session-owned"
        );
    }
}
