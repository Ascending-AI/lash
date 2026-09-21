//! The effect family: the durable runtime-effect journal and its scope fences.
//!
//! Four tables — [`replay`], [`group`], [`group_child`] and
//! [`scope_retirement`] — plus the two statements below, which read across the
//! journal and therefore belong to the family rather than to any one table
//! module.

pub mod group;
pub mod group_child;
pub mod replay;
pub mod scope_retirement;

crate::statements! {
    /// Journal-wide reads both backends issue verbatim.
    ///
    /// [`EffectJournalStatements::scope_is_quiescent`] also reads
    /// [`crate::wait::waits`]: quiescence is the one question whose answer
    /// spans both families, and splitting it into three statements would let a
    /// child start between them.
    pub struct EffectJournalStatements @ "effect_journal" {
        /// Every scope key in the journal that belongs to no session: the
        /// candidate set for a deferred scope retirement.
        select_session_free_scope_ids = "SELECT scope_id FROM runtime_effect_replay
             WHERE session_id IS NULL
             UNION
             SELECT scope_id FROM runtime_effect_group
             WHERE session_id IS NULL";

        /// Whether anything under `?1` (scope key) / `?2` (scope JSON) is
        /// still live: an `in_progress` effect row, a group still short of a
        /// journaled child, or an unresolved promise. Callers negate it —
        /// the statement answers *live*, which is the cheap half to compute.
        scope_is_quiescent = "SELECT EXISTS(
                SELECT 1 FROM runtime_effect_replay
                WHERE scope_id = ?1 AND status = 'in_progress'
             ) OR EXISTS(
                SELECT 1 FROM runtime_effect_group AS grp
                WHERE grp.scope_id = ?1
                  AND grp.children > (
                      SELECT COUNT(*) FROM runtime_effect_replay AS child
                      WHERE child.scope_id = ?1 AND child.group_key = grp.group_key
                  )
             ) OR EXISTS(
                SELECT 1 FROM await_event_waits
                WHERE scope_json = ?2 AND terminal_json IS NULL
             )";
    }
}
