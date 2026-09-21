pub fn invalid_process_key_reason(value: &str) -> Option<&'static str> {
    if value.trim().is_empty() {
        Some("process id must be a non-empty string")
    } else if value.contains('\0') {
        Some("process id must not contain NUL")
    } else if value.contains('#') {
        Some("process id contains reserved segment separator `#`")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact refusal that made a `#` in a minted identity fatal.
    ///
    /// The chain, measured during FIG-3394: a Lashlang host mints a tool-call
    /// id under its opener; the subagent spawn tool builds the child's id as
    /// `ProcessId::from(format!("process:subagent:{call_id}"))`
    /// (`crates/lash-subagents/src/rlm.rs`); registration validates that id
    /// through this function (`runtime/process/validation.rs`), as does every
    /// store read and write (`lash-sqlite-store/src/process_registry/support.rs`,
    /// `lash-postgres-store/src/postgres/process_helpers.rs`). So an opener
    /// rendered with `#` did not misparse anywhere — the child process could
    /// not be registered at all, the spawn tool failed, and the parent's
    /// driver kept asking the provider until it hit its turn cap.
    ///
    /// This is the refusal working as designed. It is pinned here so the
    /// reserved character has a test that names what depends on it.
    #[test]
    fn a_process_id_carrying_a_hash_rendered_opener_is_refused() {
        let call_id = "lashlang:process:worker#1:resource:tool:spawn:node:1";
        assert_eq!(
            invalid_process_key_reason(&format!("process:subagent:{call_id}")),
            Some("process id contains reserved segment separator `#`")
        );
    }

    /// The colon-separated rendering the same opener takes instead.
    #[test]
    fn a_process_id_carrying_a_colon_rendered_opener_is_admitted() {
        let call_id = "lashlang:process:worker:incarnation:1:resource:tool:spawn:node:1";
        assert_eq!(
            invalid_process_key_reason(&format!("process:subagent:{call_id}")),
            None
        );
    }
}
