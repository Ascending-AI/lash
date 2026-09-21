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

use lash_core::{EffectOpener, ProcessRef};
use lashlang::LashlangExecutionCallSite;

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

    /// The opener scope, canonically encoded.
    ///
    /// Every component is length-prefixed — the opener through
    /// [`EffectOpener::identity_encoding`], the cell's execution key here — so
    /// two different `(opener, execution)` pairs can never mint one scope. The
    /// diagnostic [`EffectOpener::render`] is not usable for this: its
    /// `:`-joined free-form components let `Turn("a:b", "c")` and
    /// `Turn("a", "b:c")` mint the same identity.
    fn scope(&self) -> String {
        match &self.execution {
            Some(execution) => format!(
                "{}:{}:{}",
                self.opener.identity_encoding(),
                execution.len(),
                execution
            ),
            None => self.opener.identity_encoding(),
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
        let node_id = &call_site.site.node_id;
        format!(
            "lashlang:{}:resource:{}:{}:{}:{}:{}",
            self.scope(),
            host_operation.len(),
            host_operation,
            node_id.len(),
            node_id,
            call_site.occurrence,
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
    use lash_core::{
        AdmittedScope, AdmittedScopeError, ExecutionScope, ProcessId, ProcessIncarnation, SessionId,
    };
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
    /// subagent row is a `ProcessInput::SessionTurn`, and the child turn must
    /// run under `ExecutionScope::Process`. Refusing that scope took every subagent
    /// cell's first tool call out at the knees — `task.fail(...)` came back as
    /// "has no logical opener", the child's driver re-asked the provider until
    /// its cap, and the parent read `Stopped(MaxTurns)` instead of the child's
    /// own reason.
    #[test]
    fn a_cell_under_a_process_scope_opens_on_the_admitted_incarnation() {
        let admitted = process_ref("process:subagent:call-1", 3);

        let opener = EffectOpener::for_scope(&AdmittedScope::process(admitted.clone()))
            .expect("a process is an opener");

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
        let site = call_site("resource_operation:aaaa", 1);
        let identities = |incarnation| {
            LashlangHostIdentities::cell(
                EffectOpener::for_scope(&AdmittedScope::process(process_ref(
                    "process:subagent:call-1",
                    incarnation,
                )))
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
    /// `ExecutionScope::Process` carries the reusable name, and the admitted
    /// scope refuses to admit it without the incarnation — refused at
    /// admission, rather than silently aliasing every earlier incarnation of
    /// that name.
    #[test]
    fn a_process_scope_without_an_admitted_incarnation_is_refused() {
        let error = AdmittedScope::new(ExecutionScope::process("worker"), None)
            .expect_err("the reusable name is not admitted");

        assert!(
            matches!(
                &error,
                AdmittedScopeError::ProcessIncarnationMissing { process_id }
                    if *process_id == "worker"
            ),
            "unexpected refusal: {error}"
        );
    }

    /// An incarnation of another process cannot open this one's work.
    #[test]
    fn an_admitted_incarnation_of_another_process_is_refused() {
        let error = AdmittedScope::new(
            ExecutionScope::process("worker"),
            Some(process_ref("indexer", 1)),
        )
        .expect_err("the admitted process must be the scope's process");

        assert!(
            matches!(
                &error,
                AdmittedScopeError::ProcessPinMismatch { process_id, pinned }
                    if *process_id == "worker" && *pinned == "indexer"
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
                .contains("process:6:worker:incarnation:1"),
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

    /// A queued turn's cell opens on its drain, and two cells of one drain stay
    /// apart.
    ///
    /// A turn started with `drain_id(...)` and no `turn_id(...)` runs its whole
    /// effect tree — cells included — under `ExecutionScope::QueueDrain`:
    /// `crates/lash/src/turn.rs`'s `execution_scope` resolves to
    /// `queue_drain_scope(session, drain_id)` when no turn id exists, and that
    /// is `ExecutionScope::queue_drain` verbatim
    /// (`crates/lash-core-store/src/session_state.rs`). The drain is the
    /// opener, not a container for the turn's; one drain may run several queued
    /// turns and many cells, so the cell's own replay key is what keeps two of
    /// them apart (ADR 0099 §1).
    #[test]
    fn two_cells_of_one_queued_drain_mint_distinct_identities() {
        let scope = ExecutionScope::queue_drain("session-1", "drain-3");
        let opener = EffectOpener::for_scope(
            &AdmittedScope::unpinned(scope).expect("a non-process scope admits unpinned"),
        )
        .expect("a queued-work drain is an opener");
        assert_eq!(opener, EffectOpener::queue_drain("session-1", "drain-3"));

        let site = call_site("resource_operation:aaaa", 1);
        let first = LashlangHostIdentities::cell(opener.clone(), "exec-code:1");
        let second = LashlangHostIdentities::cell(opener.clone(), "exec-code:2");

        assert_eq!(
            first.opener(),
            second.opener(),
            "both cells belong to one drain; that is the point"
        );
        assert_ne!(
            first.leaf("tool:send", &site),
            second.leaf("tool:send", &site),
            "two cells of one drain must not mint one leaf identity"
        );
        assert_ne!(
            first.child("tool:send", &site, 0),
            second.child("tool:send", &site, 0),
            "two cells of one drain must not mint one child identity"
        );
        assert_ne!(
            opener,
            EffectOpener::for_scope(
                &AdmittedScope::unpinned(ExecutionScope::turn("session-1", "drain-3"))
                    .expect("a turn admits unpinned"),
            )
            .expect("a turn is an opener"),
            "a drain is not a turn that happens to spell its id"
        );
    }

    /// The defect the canonical encoding exists for: `:`-joined free-form
    /// components are not injective.
    ///
    /// `SessionId` and `TurnId` accept arbitrary strings, so two different
    /// tagged tuples spell one `:`-joined rendering — `Turn("a:b", "c")` and
    /// `Turn("a", "b:c")` both rendered `turn:a:b:c` and minted the same leaf
    /// identity. Typed equality on the opener never protected the derived
    /// string. `render()` keeps its readable, ambiguous shape — it is the
    /// diagnostic — while `scope`/`leaf`/`child` mint from the
    /// length-prefixed encoding.
    #[test]
    fn delimiter_bearing_turn_components_mint_distinct_identities() {
        let site = call_site("resource_operation:aaaa", 1);
        let split_early =
            LashlangHostIdentities::cell(EffectOpener::turn("a:b", "c"), "exec-code:1");
        let split_late =
            LashlangHostIdentities::cell(EffectOpener::turn("a", "b:c"), "exec-code:1");

        assert_eq!(
            split_early.opener().render(),
            split_late.opener().render(),
            "the diagnostic rendering may stay ambiguous; the identity must not"
        );
        assert_ne!(
            split_early.leaf("tool:send", &site),
            split_late.leaf("tool:send", &site),
            "two splits of `a:b:c` are different openers and must mint different leaves"
        );
        assert_ne!(
            split_early.child("tool:send", &site, 0),
            split_late.child("tool:send", &site, 0),
            "two splits of `a:b:c` are different openers and must mint different children"
        );
    }

    /// The drain arm had the same collision: `QueueDrain("a:b", "c")` and
    /// `QueueDrain("a", "b:c")` both rendered `drain:a:b:c`.
    #[test]
    fn delimiter_bearing_drain_components_mint_distinct_identities() {
        let site = call_site("resource_operation:aaaa", 1);
        let split_early =
            LashlangHostIdentities::cell(EffectOpener::queue_drain("a:b", "c"), "exec-code:1");
        let split_late =
            LashlangHostIdentities::cell(EffectOpener::queue_drain("a", "b:c"), "exec-code:1");

        assert_ne!(
            split_early.leaf("tool:send", &site),
            split_late.leaf("tool:send", &site),
            "two splits of `a:b:c` are different drains and must mint different leaves"
        );
        assert_ne!(
            split_early.child("tool:send", &site, 0),
            split_late.child("tool:send", &site, 0),
            "two splits of `a:b:c` are different drains and must mint different children"
        );
    }

    /// A session id that itself carries `:` — the shape every spawned child's
    /// session takes (`session:subagent:{call_id}`,
    /// `crates/lash-subagents/src/rlm.rs`) — round-trips through the typed
    /// opener untouched and mints an identity that cannot alias a different
    /// split of the same bytes.
    #[test]
    fn a_delimiter_bearing_session_id_round_trips() {
        let spawned_session = "session:subagent:lashlang:turn:1:x:1:y";
        let scope = ExecutionScope::turn(spawned_session, "turn-1");
        let opener = EffectOpener::for_scope(
            &AdmittedScope::unpinned(scope).expect("a non-process scope admits unpinned"),
        )
        .expect("a turn is an opener");

        assert_eq!(
            opener.session_id().map(SessionId::as_str),
            Some(spawned_session),
            "the session id round-trips through the typed opener untouched"
        );

        let site = call_site("resource_operation:aaaa", 1);
        let leaf = LashlangHostIdentities::cell(opener, "exec-code:1").leaf("tool:send", &site);
        assert!(
            leaf.contains("38:session:subagent:lashlang:turn:1:x:1:y"),
            "the canonical encoding length-prefixes the session id, keeping its `:` bytes inside one component: {leaf}"
        );
        assert_ne!(
            leaf,
            LashlangHostIdentities::cell(
                EffectOpener::turn("session:subagent:lashlang:turn:1:x:1", "y:turn-1"),
                "exec-code:1",
            )
            .leaf("tool:send", &site),
            "the same bytes split across the session/turn boundary are a different opener"
        );
    }

    /// The real embedding chain, two process ids deep.
    ///
    /// A minted call id becomes the child's whole `ProcessId`
    /// (`process:subagent:{call_id}`), which becomes the child's opener under
    /// `ExecutionScope::Process`, whose own minted call id becomes the
    /// grandchild's `ProcessId` in turn. Every link must pass
    /// `invalid_process_key_reason` — the canonical encoding may carry `:` but
    /// never `#`, which is refused inside a process id.
    #[test]
    fn a_call_id_nested_two_process_ids_deep_is_admitted() {
        let site = call_site("resource_operation:aaaa", 1);
        let parent_leaf =
            LashlangHostIdentities::cell(EffectOpener::turn("session-1", "turn-7"), "exec-code:1")
                .leaf("tool:spawn_agent", &site);

        let child_process_id = ProcessId::from(format!("process:subagent:{parent_leaf}"));
        assert_eq!(
            lash_core::store::process_key::invalid_process_key_reason(child_process_id.as_str()),
            None,
            "a minted call id embedded in a child process id must be registrable"
        );

        let child_opener = EffectOpener::for_scope(&AdmittedScope::process(process_ref(
            child_process_id.as_str(),
            4,
        )))
        .expect("the spawned child process is an opener");
        let grandchild_leaf = LashlangHostIdentities::cell(child_opener, "exec-code:1")
            .leaf("tool:spawn_agent", &site);
        let grandchild_process_id = ProcessId::from(format!("process:subagent:{grandchild_leaf}"));

        assert_eq!(
            lash_core::store::process_key::invalid_process_key_reason(
                grandchild_process_id.as_str()
            ),
            None,
            "a call id nested two process ids deep must still be registrable: \
             {grandchild_process_id}"
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
