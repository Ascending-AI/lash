//! Scope-exact effect-journal retirement: the lever a host reaches for when a
//! process or runtime operation can never run again, and the proofs it
//! carries (FIG-2499, FIG-2500).

use lash::durability::{EffectHost, EffectJournalRetirement, EffectRetirementGate};
use lash::runtime::ExecutionScope;

// docs:start:retire_scope
/// Retire the journal of a runtime operation the host has proven finished,
/// and read back which scope the fence will cover.
pub async fn retire_finished_operation(
    host: &dyn EffectHost,
    operation_id: &str,
) -> Result<usize, lash::runtime::RuntimeError> {
    let scope = ExecutionScope::runtime_operation(operation_id);
    // `for_scope` names the exact scope; a session-bearing scope yields
    // `None` because sessions retire through session deletion instead.
    let retirement = EffectJournalRetirement::for_scope(&scope)
        .expect("runtime-operation scopes retire through the scope lever");
    assert_eq!(retirement.retired_scope(), Some(scope));
    // The constructors carry the owner-terminal proof: the caller knows the
    // owner is gone, so in-flight rows go too.
    assert_eq!(retirement.gate(), Some(EffectRetirementGate::OwnerTerminal));
    host.retire_effect_journal(retirement).await
}

/// Retire a scope only once the store itself proves nothing is live under
/// it; a scope with a draining child reports `effect_scope_not_quiescent`
/// and is retried later.
pub async fn retire_when_quiescent(
    host: &dyn EffectHost,
    operation_id: &str,
) -> Result<Option<usize>, lash::runtime::RuntimeError> {
    let retirement = EffectJournalRetirement::runtime_operation(operation_id).when_quiescent();
    assert_eq!(retirement.gate(), Some(EffectRetirementGate::WhenQuiescent));
    match host.retire_effect_journal(retirement).await {
        Ok(deleted) => Ok(Some(deleted)),
        Err(err) if err.code == lash::runtime::RuntimeErrorCode::EffectScopeNotQuiescent => {
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

/// A host that reuses process ids lifts the fence a prune left before the
/// new incarnation runs; the facade does this inside `Processes::start`.
pub async fn reinstate_reused_process_id(
    host: &dyn EffectHost,
    process_id: &str,
) -> Result<(), lash::runtime::RuntimeError> {
    host.reinstate_effect_scope(&ExecutionScope::process(process_id))
        .await
}
// docs:end:retire_scope

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scope_retirement_snippets_run_against_the_in_memory_host() {
        let host = lash::durability::NativeEffectHost::default();
        assert_eq!(
            retire_finished_operation(&host, "docs-op")
                .await
                .expect("retire"),
            0
        );
        assert_eq!(
            retire_when_quiescent(&host, "docs-op-2")
                .await
                .expect("retire when quiescent"),
            Some(0)
        );
        reinstate_reused_process_id(&host, "docs-process")
            .await
            .expect("reinstate");
    }
}
