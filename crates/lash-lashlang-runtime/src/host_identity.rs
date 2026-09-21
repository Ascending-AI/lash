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
/// `RuntimeOperation`, which run no cells at all. These are refusals rather
/// than extra arms: widening the opener is a contract decision, and inventing
/// an identity here would hide the site that needed it.
#[derive(Debug, thiserror::Error)]
pub enum LashlangOpenerError {
    /// A scope kind that is not an opener at all.
    #[error(
        "lashlang execution has no logical opener: {scope_kind} scope names neither a turn, a queued-work drain nor a process incarnation"
    )]
    NotAnOpener {
        /// The scope kind, for the diagnostic.
        scope_kind: &'static str,
    },
    /// A process scope whose run bound no incarnation.
    ///
    /// `ExecutionScope::Process` carries the reusable process *name*, so the
    /// name alone cannot be the opener: it would alias every earlier
    /// incarnation's groups, closes and cancellation fences (ADR 0099 §1).
    /// The process runner binds the admitted incarnation onto the scoped
    /// effect controller, so this refusal means the execution did not come
    /// through a process runner at all.
    #[error(
        "lashlang execution under process `{process_id}` was not admitted with an incarnation, so it has no logical opener"
    )]
    ProcessWithoutIncarnation {
        /// The reusable process name the scope carried.
        process_id: String,
    },
    /// A process scope carrying an incarnation of some other process.
    #[error("lashlang execution under process `{process_id}` was admitted as process `{admitted}`")]
    ProcessMismatch {
        /// The process the scope names.
        process_id: String,
        /// The process the admitted incarnation names.
        admitted: String,
    },
}

/// The opener a cell's scope names, or a refusal.
///
/// A queued turn is a real production shape, not an edge one: a turn started
/// with `drain_id` and no turn id runs its whole effect tree — cells
/// included — under `ExecutionScope::QueueDrain` (`crates/lash/src/turn.rs`,
/// `execution_scope`), so a drain is an opener in its own right.
///
/// So is a cell under a process scope. A `ProcessInput::SessionTurn` row — the
/// shape every `agents.spawn` child takes — runs a whole child session turn
/// under `ExecutionScope::Process`
/// (`SessionTurnRequest::new_process_backed` requires exactly that scope), and
/// that turn's cells are opened by the process, not by the child turn: a
/// worker retry keeps the incarnation and reuses the journal, while a
/// re-registration is a different opener. The scope alone cannot say which,
/// which is why `admitted_process` is an argument here: it is the incarnation
/// the process runner bound onto the scoped controller, and its absence is
/// refused rather than filled in with the reusable name.
pub fn cell_opener_for_scope(
    scope: &ExecutionScope,
    admitted_process: Option<&ProcessRef>,
) -> Result<EffectOpener, LashlangOpenerError> {
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
        ExecutionScope::Process { process_id } => match admitted_process {
            Some(process_ref) if process_ref.process_id == *process_id => {
                Ok(EffectOpener::process(process_ref.clone()))
            }
            Some(process_ref) => Err(LashlangOpenerError::ProcessMismatch {
                process_id: process_id.to_string(),
                admitted: process_ref.process_id.to_string(),
            }),
            None => Err(LashlangOpenerError::ProcessWithoutIncarnation {
                process_id: process_id.to_string(),
            }),
        },
        ExecutionScope::SessionDelete { .. } => Err(LashlangOpenerError::NotAnOpener {
            scope_kind: "session-delete",
        }),
        ExecutionScope::RuntimeOperation { .. } => Err(LashlangOpenerError::NotAnOpener {
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

    fn process_ref(name: &str, incarnation: u64) -> ProcessRef {
        ProcessRef::new(
            name,
            ProcessIncarnation::from_registration_sequence(incarnation),
        )
    }

    /// A cell of a process-backed session turn is opened by its process.
    ///
    /// This is the production shape every `agents.spawn` child takes: the
    /// subagent row is a `ProcessInput::SessionTurn`, and
    /// `SessionTurnRequest::new_process_backed` requires the child turn to run
    /// under `ExecutionScope::Process`. Refusing that scope took every subagent
    /// cell's first tool call out at the knees — `task.fail(...)` came back as
    /// "has no logical opener", the child's driver re-asked the provider until
    /// its cap, and the parent read `Stopped(MaxTurns)` instead of the child's
    /// own reason.
    #[test]
    fn a_cell_under_a_process_scope_opens_on_the_admitted_incarnation() {
        let scope = ExecutionScope::process("process:subagent:call-1");
        let admitted = process_ref("process:subagent:call-1", 3);

        let opener =
            cell_opener_for_scope(&scope, Some(&admitted)).expect("a process is an opener");

        assert_eq!(opener, EffectOpener::process(admitted));
        assert_eq!(
            opener.render(),
            "process:process:subagent:call-1:incarnation:3"
        );
    }

    /// The incarnation is what keeps a reused process name apart, so two
    /// incarnations of one process-backed turn mint different identities while
    /// a worker retry of the same incarnation mints the same ones.
    #[test]
    fn two_incarnations_of_one_process_backed_cell_mint_distinct_identities() {
        let scope = ExecutionScope::process("process:subagent:call-1");
        let site = call_site("resource_operation:aaaa", 1);
        let identities = |incarnation| {
            LashlangHostIdentities::cell(
                cell_opener_for_scope(
                    &scope,
                    Some(&process_ref("process:subagent:call-1", incarnation)),
                )
                .expect("a process is an opener"),
                "cell:1",
            )
        };

        assert_ne!(
            identities(1).leaf("tool:task.fail", &site),
            identities(2).leaf("tool:task.fail", &site),
            "a re-registered process is a different opener (ADR 0099 §1)"
        );
        assert_eq!(
            identities(1).leaf("tool:task.fail", &site),
            identities(1).leaf("tool:task.fail", &site),
            "a worker retry keeps the incarnation, so it re-derives the same identity"
        );
    }

    /// The name alone is never the opener.
    ///
    /// `ExecutionScope::Process` carries the reusable name, and a run that
    /// reached here without a process runner binding its admitted incarnation
    /// has no opener to mint under — refused, rather than silently aliasing
    /// every earlier incarnation of that name.
    #[test]
    fn a_process_scope_without_an_admitted_incarnation_is_refused() {
        let error = cell_opener_for_scope(&ExecutionScope::process("worker"), None)
            .expect_err("the reusable name is not an opener");

        assert!(
            matches!(
                &error,
                LashlangOpenerError::ProcessWithoutIncarnation { process_id } if process_id == "worker"
            ),
            "unexpected refusal: {error}"
        );
    }

    /// An incarnation of another process cannot open this one's work.
    #[test]
    fn an_admitted_incarnation_of_another_process_is_refused() {
        let error = cell_opener_for_scope(
            &ExecutionScope::process("worker"),
            Some(&process_ref("indexer", 1)),
        )
        .expect_err("the admitted process must be the scope's process");

        assert!(
            matches!(
                &error,
                LashlangOpenerError::ProcessMismatch { process_id, admitted }
                    if process_id == "worker" && admitted == "indexer"
            ),
            "unexpected refusal: {error}"
        );
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
