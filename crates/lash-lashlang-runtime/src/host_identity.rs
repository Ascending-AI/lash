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
//! call. Keeping the rendering in one place is what makes the authority an
//! argument instead of a property of whichever host happened to build the
//! string.
//!
//! The authority stays an explicit choice rather than a derived one: a turn's
//! cell and a process body are different openers, and an identity minted under
//! one must never be reachable from the other even when the same module runs
//! both ways.

use lash_core::{ProcessId, ProcessRef};
use lashlang::LashlangExecutionCallSite;

/// The logical opener whose keys a Lashlang host is minting.
///
/// ADR 0099 §1: an opener is a turn or a **process incarnation**, its identity
/// is stable across worker attempts and segments, and it changes on process
/// re-registration. Nothing below may be derived from a worker attempt, a lease
/// or a segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LashlangHostAuthority {
    /// A cell running inside a turn.
    ///
    /// The scope is the effect address the cell runs under, whose graph key
    /// renders the turn opener — `"kind":"turn"` with the session id and the
    /// turn's execution id — together with the cell's own replay key. It is
    /// therefore already a binding of the opener and then some: two cells of
    /// one turn are separated, and no two turns can alias. A cell that runs
    /// outside an effect falls back to the session id, which the caller
    /// resolves because only it can see its own invocation.
    Turn(String),
    /// A logical process incarnation, as `ProcessRef` pins it: a reusable
    /// process *name* bound to one store-minted incarnation.
    ///
    /// The name alone is not the opener. `ExecutionScope::Process` carries only
    /// `process_id` (crates/lash-sansio/src/effect_identity.rs), so a process
    /// re-registered under the same name would mint the identities its
    /// predecessor already used and alias a prior group, close or cancel fence
    /// — which is exactly what ADR 0099 §1 refuses. The incarnation is bound
    /// here, rendered the way ADR 0094 renders a process parent scope.
    Process(ProcessRef),
}

impl LashlangHostAuthority {
    /// The process opener, from the name and the incarnation the store minted
    /// for it.
    pub fn process(
        process_id: impl Into<ProcessId>,
        incarnation: lash_core::ProcessIncarnation,
    ) -> Self {
        Self::Process(ProcessRef::new(process_id, incarnation))
    }

    /// The rendered opener, tagged by kind.
    ///
    /// The tag is not decoration: a turn scope is a free-form string (an effect
    /// graph key, or a host-chosen session id when the cell runs outside an
    /// effect) and could spell `{process_id}#{incarnation}` exactly. Without
    /// the tag the two openers would mint one identity, which is the aliasing
    /// ADR 0099 §1 refuses.
    fn scope(&self) -> String {
        match self {
            Self::Turn(scope) => format!("turn:{scope}"),
            Self::Process(process_ref) => format!(
                "process:{}:incarnation:{}",
                process_ref.process_id, process_ref.incarnation
            ),
        }
    }
}

/// The identities one Lashlang host mints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LashlangHostIdentities {
    authority: LashlangHostAuthority,
}

impl LashlangHostIdentities {
    pub fn new(authority: LashlangHostAuthority) -> Self {
        Self { authority }
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
            self.authority.scope(),
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
        LashlangHostIdentities::new(LashlangHostAuthority::process(
            name,
            lash_core::ProcessIncarnation::from_registration_sequence(incarnation),
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
        assert!(
            !first.leaf("tool:send", &site).contains('#'),
            "`#` is ADR 0094's process parent-scope separator; a minted \
             identity that carries one is ambiguous with it, and the subagent \
             spawn tool embeds this id verbatim in a child ProcessId"
        );
    }

    /// A turn cell and a process body that run the same program are different
    /// openers, so no identity is reachable from both.
    #[test]
    fn a_turn_and_a_process_never_share_an_identity() {
        let site = call_site("resource_operation:aaaa", 1);
        let turn = LashlangHostIdentities::new(LashlangHostAuthority::Turn("worker#1".to_string()));

        assert_ne!(
            turn.leaf("tool:send", &site),
            process_opener("worker", 1).leaf("tool:send", &site),
            "a turn scope that spells a process opener must still not collide with it"
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
