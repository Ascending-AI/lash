use lash_core_store::scope_identity::ScopeId;
use lash_durable::domain::ScopeKey;
/// The registry's scope for `scope`: what its `Until` children name and
/// what its closure fact is keyed by. A code cell owns no `Until`
/// processes.
#[must_use]
pub fn scope_id(scope: &ScopeKey) -> Option<ScopeId> {
    Some(match scope {
        ScopeKey::Turn(session_id, turn_id) => lash_core_store::scope_identity::ScopeId::Opener(
            lash_core_store::effect_opener::EffectOpener::Turn {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
            },
        ),
        ScopeKey::Operation(session_id, operation_id) => {
            lash_core_store::scope_identity::ScopeId::Opener(
                lash_core_store::effect_opener::EffectOpener::session_operation(
                    session_id.clone(),
                    operation_id.clone(),
                ),
            )
        }
        ScopeKey::Session(session_id) => {
            lash_core_store::scope_identity::ScopeId::Session(session_id.clone())
        }
        ScopeKey::Process(process_id) => lash_core_store::scope_identity::ScopeId::Opener(
            lash_core_store::effect_opener::EffectOpener::Process {
                process_id: process_id.clone(),
            },
        ),
        ScopeKey::Cell(..) => return None,
    })
}

/// The registry's index columns for `scope`, `(lifetime_scope_kind,
/// lifetime_scope_id)`: the projection its `Until` children are found by.
#[must_use]
pub fn scope_index(scope: &ScopeKey) -> Option<(&'static str, String)> {
    scope_id(scope).map(|scope| (scope.storage_kind(), scope.storage_id()))
}

/// The roots of `scope`'s `Until` subtree, as the live-subtree read binds
/// them: the scope's own index columns and, for a session, the index-id
/// prefixes of the turn and session-operation scopes inside it, whose
/// processes live in the session's tree too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubtreeRoots {
    /// The scope's `lifetime_scope_kind`.
    pub kind: &'static str,
    /// The scope's `lifetime_scope_id`.
    pub id: String,
    /// For a session, the prefix of its turn scopes' ids.
    pub turns: Option<String>,
    /// For a session, the prefix of its session-operation scopes' ids.
    pub operations: Option<String>,
}

/// The roots of `scope`'s `Until` subtree; `None` for a code cell.
#[must_use]
pub fn subtree_roots(scope: &ScopeKey) -> Option<SubtreeRoots> {
    let (kind, id) = scope_index(scope)?;
    let (turns, operations) = match scope {
        // Each range's lower bound is the prefix every encoding in it
        // starts with.
        ScopeKey::Session(session_id) => (
            Some(
                lash_core_store::effect_opener::EffectOpener::session_turn_encoding_range(
                    session_id,
                )
                .0,
            ),
            Some(
                lash_core_store::effect_opener::EffectOpener::session_operation_encoding_range(
                    session_id,
                )
                .0,
            ),
        ),
        ScopeKey::Turn(..)
        | ScopeKey::Operation(..)
        | ScopeKey::Process(_)
        | ScopeKey::Cell(..) => (None, None),
    };
    Some(SubtreeRoots {
        kind,
        id,
        turns,
        operations,
    })
}
