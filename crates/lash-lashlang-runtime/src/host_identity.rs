//! One derivation of the identities a Lashlang host mints for the work a
//! program asks it to start: a leaf call, a child of an aggregate, and the
//! aggregate's own group.
//!
//! There are two Lashlang hosts — the RLM cell bridge and the process body
//! bridge — and they had a `resource_tool_call_id` each. The two spellings had
//! already drifted: the process host scoped every identity on its process id,
//! while the cell host scoped on the effect address it ran under *only when a
//! leaf carried a call site* and fell back to the bare session id otherwise, so
//! two cells of one session minted the same identity for their first unsited
//! call. Keeping the rendering in one place is what makes the opener an
//! argument instead of a property of whichever host happened to build the
//! string.

use lash_core::{EffectOpener, ExecutionScope, ProcessRef};
use lashlang::LashlangExecutionCallSite;

/// A scope that names no opener this contract can express.
///
/// ADR 0099 §1 knows three openers — a turn, a queued-work drain and a process
/// incarnation — and `ExecutionScope` has two more kinds, `SessionDelete` and
/// `RuntimeOperation`, which run no cells at all. This is a refusal rather
/// than a fourth arm: widening the opener is a contract decision, and
/// inventing an identity here would hide the site that needed it.
#[derive(Debug, thiserror::Error)]
#[error(
    "lashlang execution has no logical opener: {scope_kind} scope names neither a turn nor a process incarnation"
)]
pub struct LashlangOpenerError {
    scope_kind: &'static str,
}

/// The opener a cell's scope names, or a refusal.
///
/// A queued turn is a real production shape, not an edge one: a turn started
/// with `drain_id` and no turn id runs its whole effect tree — cells
/// included — under `ExecutionScope::QueueDrain` (`crates/lash/src/turn.rs`,
/// `execution_scope`), so a drain is an opener in its own right.
///
/// A process scope cannot answer here: `ExecutionScope::Process` carries the
/// reusable name and not the store-minted incarnation, so a process body builds
/// its opener from the incarnation its run was admitted under instead
/// ([`LashlangHostIdentities::process_body`]).
pub fn cell_opener_for_scope(scope: &ExecutionScope) -> Result<EffectOpener, LashlangOpenerError> {
    match scope {
        ExecutionScope::Turn {
            session_id,
            turn_id,
        } => Ok(EffectOpener::turn(session_id.clone(), turn_id.clone())),
        ExecutionScope::QueueDrain {
            session_id,
            drain_id,
        } => Ok(EffectOpener::queue_drain(
            session_id.clone(),
            drain_id.clone(),
        )),
        ExecutionScope::Process { .. } => Err(LashlangOpenerError {
            scope_kind: "process",
        }),
        ExecutionScope::SessionDelete { .. } => Err(LashlangOpenerError {
            scope_kind: "session-delete",
        }),
        ExecutionScope::RuntimeOperation { .. } => Err(LashlangOpenerError {
            scope_kind: "runtime-operation",
        }),
    }
}

/// The identities one Lashlang host mints.
///
/// Two facts, and only one of them is the opener. [`EffectOpener`] is the
/// lifecycle owner (ADR 0099 §1) — a turn, or one process incarnation — and it
/// is the shared type, never a second spelling of it. `execution` is the part
/// of the identity the opener is deliberately too coarse to supply: a turn runs
/// many cells, and two cells of one turn running the same program would
/// otherwise mint the same leaf ids, because a leaf id is a node id plus an
/// occurrence counted per VM execution. A process body has no such
/// subdivision — it is one execution for its whole life, across every segment —
/// so it carries none, and a segment must never appear here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LashlangHostIdentities {
    opener: EffectOpener,
    execution: Option<String>,
}

impl LashlangHostIdentities {
    /// The identities one cell of a turn mints.
    ///
    /// `execution_key` is the cell's own replay key inside the turn.
    pub fn cell(opener: EffectOpener, execution_key: impl Into<String>) -> Self {
        Self {
            opener,
            execution: Some(execution_key.into()),
        }
    }

    /// The identities one process body mints, for the whole life of the
    /// incarnation.
    pub fn process_body(process_ref: ProcessRef) -> Self {
        Self {
            opener: EffectOpener::process(process_ref),
            execution: None,
        }
    }

    /// The opener every identity below binds.
    pub fn opener(&self) -> &EffectOpener {
        &self.opener
    }

    fn scope(&self) -> String {
        match &self.execution {
            Some(execution) => format!("{}:{execution}", self.opener.render()),
            None => self.opener.render(),
        }
    }

    /// The identity of one call the program made on its own.
    ///
    /// The call site is required, not preferred. Every production compile
    /// entrypoint for both bridges enables execution-site tracking, and
    /// `lashlang_execution_paths` walks `program.main` through the total
    /// `Expr::children()` walk, so a tool leaf without a site does not exist:
    /// a caller that cannot supply one has a defect upstream, and both bridges
    /// refuse it with `LashlangHostError::OperationCallSiteMissing` rather than
    /// inventing a position. A fallback here would be a second identity
    /// grammar for a case that cannot arise, and the one that existed — the
    /// leaf's position inside its batch — could not tell two identical
    /// aggregates apart.
    pub fn leaf(&self, host_operation: &str, call_site: &LashlangExecutionCallSite) -> String {
        format!(
            "lashlang:{}:resource:{host_operation}:{}:{}",
            self.scope(),
            call_site.site.node_id,
            call_site.occurrence
        )
    }

    /// The identity of one leaf of an aggregate, at `leaf_index` of the batch
    /// the program wrote.
    ///
    /// The index is the leaf's position in the aggregate as written, not the
    /// order it settled in: it has to be the same on a replay that settles the
    /// leaves in another order.
    pub fn child(
        &self,
        host_operation: &str,
        call_site: &LashlangExecutionCallSite,
        leaf_index: usize,
    ) -> String {
        format!(
            "{}:child:{leaf_index}",
            self.leaf(host_operation, call_site)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::ProcessIncarnation;
    use lashlang::{LashlangExecutionSite, WorkflowExecutionSite};

    fn call_site(node_id: &str, occurrence: u64) -> LashlangExecutionCallSite {
        LashlangExecutionCallSite {
            site: LashlangExecutionSite {
                node_id: node_id.to_string(),
                node_kind: "resource_operation".to_string(),
                label: "call".to_string(),
                branch: None,
                workflow_site: WorkflowExecutionSite::new(
                    "main",
                    [0u32],
                    "resource_operation",
                    "call",
                ),
            },
            occurrence,
        }
    }

    fn process_opener(name: &str, incarnation: u64) -> LashlangHostIdentities {
        LashlangHostIdentities::process_body(ProcessRef::new(
            name,
            ProcessIncarnation::from_registration_sequence(incarnation),
        ))
    }

    /// ADR 0099 §1: a re-registered process name is a different opener.
    ///
    /// Red on the parent commit, where the process tier scoped every identity
    /// on the process id alone: the second incarnation re-minted the first
    /// one's keys, so a group, a close or a cancellation fence the predecessor
    /// left behind was reachable from a process that only happens to carry the
    /// same name.
    #[test]
    fn a_re_registered_process_name_is_a_different_opener() {
        let site = call_site("resource_operation:aaaa", 1);
        let first = process_opener("worker", 1);
        let second = process_opener("worker", 2);

        assert_ne!(
            first.leaf("tool:send", &site),
            second.leaf("tool:send", &site),
            "two incarnations of one process name must not share a leaf identity"
        );
        assert_ne!(
            first.child("tool:send", &site, 0),
            second.child("tool:send", &site, 0),
            "two incarnations of one process name must not share a child identity"
        );
        assert!(
            first
                .leaf("tool:send", &site)
                .contains("process:worker:incarnation:1"),
            "the incarnation is bound, not merely mixed in"
        );
    }

    /// A minted identity must carry neither reserved separator.
    ///
    /// `#` is refused outright inside a process id
    /// (`invalid_process_key_reason`), and the subagent spawn tool builds a
    /// child `ProcessId` out of one of these call ids verbatim, so a `#` here
    /// makes the child unregistrable rather than merely ugly.
    #[test]
    fn a_minted_identity_carries_no_reserved_separator() {
        let minted = process_opener("worker", 1).leaf("tool:send", &call_site("node:aaaa", 1));
        assert!(!minted.contains('#'), "{minted}");
        assert!(!minted.contains('/'), "{minted}");
    }

    /// A turn cell and a process body that run the same program are different
    /// openers, so no identity is reachable from both.
    #[test]
    fn a_turn_and_a_process_never_share_an_identity() {
        let site = call_site("resource_operation:aaaa", 1);
        let turn = LashlangHostIdentities::cell(
            EffectOpener::turn("process:worker:incarnation:1", "t"),
            "exec-code:1",
        );

        assert_ne!(
            turn.leaf("tool:send", &site),
            process_opener("worker", 1).leaf("tool:send", &site),
            "a turn whose ids spell a process opener must still not collide with it"
        );
    }

    /// The defect the cell key exists to prevent: one turn, two cells, one
    /// program.
    ///
    /// A leaf id is a node id plus an occurrence counted per VM execution, and
    /// each cell gets a fresh VM, so two cells of one turn running the same
    /// source produce the same node id at the same occurrence. The opener is
    /// the same for both — it is the turn — so without the cell's own
    /// execution key the two mint one identity.
    #[test]
    fn two_cells_of_one_turn_running_one_program_mint_distinct_identities() {
        let site = call_site("resource_operation:aaaa", 1);
        let opener = EffectOpener::turn("session-1", "turn-7");
        let first = LashlangHostIdentities::cell(opener.clone(), "exec-code:1");
        let second = LashlangHostIdentities::cell(opener.clone(), "exec-code:2");

        assert_eq!(
            first.opener(),
            second.opener(),
            "both cells belong to one opener; that is the point"
        );
        assert_ne!(
            first.leaf("tool:send", &site),
            second.leaf("tool:send", &site),
            "two cells of one turn must not mint one leaf identity"
        );
        assert_ne!(
            first.child("tool:send", &site, 0),
            second.child("tool:send", &site, 0),
            "two cells of one turn must not mint one child identity"
        );
    }

    /// Two reaches of one aggregate are separated by the site occurrence the
    /// VM counted, and the leaves of one reach by their position in it.
    #[test]
    fn one_aggregate_reached_twice_mints_four_child_identities() {
        let identities = process_opener("worker", 1);
        let minted = [1u64, 2]
            .into_iter()
            .flat_map(|occurrence| {
                let identities = &identities;
                [0usize, 1].into_iter().map(move |leaf_index| {
                    identities.child(
                        "tool:send",
                        &call_site("resource_operation:aaaa", occurrence),
                        leaf_index,
                    )
                })
            })
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(
            minted.len(),
            4,
            "two leaves of two aggregate occurrences are four identities"
        );
    }
}
