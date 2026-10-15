//! What a live child turn that runs inside a process carries about it: the
//! process's lineage.

#[derive(Clone)]
struct ProcessTurnCorrelation {
    /// The lineage of the process the turn runs inside (FIG-3607 R1): what a
    /// start made in that turn records above its starter.
    lineage: crate::ProcessLineage,
}

/// Carries the lineage of the process a live child turn runs inside.
pub(crate) fn attach_process_lineage(
    turn_context: &mut crate::TurnContext,
    lineage: crate::ProcessLineage,
) {
    turn_context.set_runtime_correlation(ProcessTurnCorrelation { lineage });
}

/// The lineage a turn context carries, when its turn runs inside a process.
pub(crate) fn process_lineage_of(
    turn_context: &crate::TurnContext,
) -> Option<crate::ProcessLineage> {
    turn_context
        .runtime_correlation::<ProcessTurnCorrelation>()
        .map(|correlation| correlation.lineage.clone())
}
