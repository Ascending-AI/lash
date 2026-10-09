//! The identities a Lash VM host mints for the work a program asks it to
//! start: a call's id, the id of one leaf of an aggregate, and the key
//! namespace every journal row of the run lives under.
//!
//! The RLM cell bridge mints from here, with the opener as an argument
//! instead of a property of whichever host happened to build the string. The
//! call ids themselves come from [`CodeCallIdentities`], the one derivation
//! the worker broker mints from too (ADR 0117 §2, ADR 0123). A process body
//! mints its call ids from [`CodeCallIdentities::process_body`] and journals
//! nothing: its operations are admitted by operation id (ADR 0132 §8).
//!
//! Every identity is positional (FIG-3586): a command is named by the issue
//! ordinal it took when it left the VM, never by the call site that issued
//! it. The call site's node id and occurrence are trace metadata only.

use lash_core::EffectOpener;
use lash_vm_broker::CodeCallIdentities;

use crate::replay_run::LashVmReplayNamespace;

/// The identities one Lash VM host mints: the code call identities of the
/// run, plus the replay namespace the in-process host journals under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LashVmHostIdentities {
    code: CodeCallIdentities,
    execution: String,
}

impl LashVmHostIdentities {
    /// The identities one cell of a turn mints.
    ///
    /// `execution_key` is the cell's own replay key inside the turn.
    pub fn cell(opener: EffectOpener, execution_key: impl Into<String>) -> Self {
        let execution = execution_key.into();
        Self {
            code: CodeCallIdentities::cell(opener, execution.clone()),
            execution,
        }
    }

    /// The opener every identity below binds.
    pub fn opener(&self) -> &EffectOpener {
        self.code.opener()
    }

    /// The code call identities the broker mints the same ids from.
    pub fn code(&self) -> &CodeCallIdentities {
        &self.code
    }

    /// The key namespace every journal row of this run lives under: the
    /// cell's own replay key, beside the rest of its turn's journal.
    pub fn namespace(&self) -> LashVmReplayNamespace {
        LashVmReplayNamespace::cell(&self.execution)
    }

    /// The id of the call the program issued at `ordinal`: the tool call's
    /// id, and the reply id of an awaited handle.
    ///
    /// A child-spawning tool keys its child's start on this id (through
    /// its recorded call), so it is what keeps a redriven spawn from starting
    /// a second child: it moves only when the command's position in the run
    /// moves.
    pub fn call_id(&self, ordinal: u64) -> lash_core::ToolCallId {
        self.code.call_id(ordinal)
    }

    /// The id of the leaf at `leaf_index` (its first-appearance index in the
    /// aggregate as written, not the order it settled in) of the aggregate
    /// the program issued at `ordinal`.
    pub fn child_call_id(&self, ordinal: u64, leaf_index: usize) -> lash_core::ToolCallId {
        self.code.child_call_id(ordinal, leaf_index as u64)
    }

    /// The replay key of the run's one durable effect-omission record.
    pub fn effect_omissions(&self) -> String {
        format!("lash_vm:{}:effect_omissions", self.code.scope())
    }
}

impl lash_core::store::DurableRecord for LashVmHostIdentities {
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::replay_run::LASH_VM_REPLAY_KEY_GRAMMAR_VERSION);
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::{AdmittedScope, ExecutionScope, ProcessId, SessionId};

    fn process_id(label: &str) -> ProcessId {
        lash_core::process_id_for_test(label)
    }

    fn process_opener(label: &str) -> CodeCallIdentities {
        CodeCallIdentities::process_body(process_id(label))
    }

    /// The minted id is what keeps two processes apart, so two processes
    /// running one process-backed turn mint different identities while a
    /// worker retry of the same process mints the same ones.
    #[test]
    fn two_processes_running_one_process_backed_cell_mint_distinct_identities() {
        let identities = |label| {
            LashVmHostIdentities::cell(
                EffectOpener::for_scope(&AdmittedScope::process(process_id(label)))
                    .expect("a process is an opener"),
                "cell:1",
            )
        };

        assert_ne!(
            identities("first").call_id(0),
            identities("second").call_id(0),
            "two processes are two openers (ADR 0099 §1)"
        );
        assert_eq!(
            identities("first").call_id(0),
            identities("first").call_id(0),
            "a worker retry keeps the process id, so it re-derives the same identity"
        );
    }

    /// ADR 0099 §1: two processes are two openers, even when a host labels
    /// them alike. The minted id is the whole opener; nothing a caller names
    /// reaches it.
    #[test]
    fn two_processes_are_two_openers() {
        let first = process_opener("worker-a");
        let second = process_opener("worker-b");

        assert_ne!(
            first.call_id(0),
            second.call_id(0),
            "two processes must not share a leaf identity"
        );
        assert_ne!(
            first.child_call_id(0, 0),
            second.child_call_id(0, 0),
            "two processes must not share a child identity"
        );
    }

    /// A minted identity must carry neither reserved separator.
    ///
    /// A call id is embedded in effect and reply ids, whose grammars reserve
    /// `#` and `/`, so a minted one must carry neither.
    #[test]
    fn a_minted_identity_carries_no_reserved_separator() {
        let minted = process_opener("worker").call_id(0).to_string();
        assert!(!minted.contains('#'), "{minted}");
        assert!(!minted.contains('/'), "{minted}");
    }

    /// A turn cell and a process body that run the same program are different
    /// openers, so no identity is reachable from both.
    #[test]
    fn a_turn_and_a_process_never_share_an_identity() {
        let turn = LashVmHostIdentities::cell(
            EffectOpener::turn("process:worker:incarnation:1", "t"),
            "exec-code:1",
        );

        assert_ne!(
            turn.call_id(0),
            process_opener("worker").call_id(0),
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
        let opener = EffectOpener::turn("session-1", "turn-7");
        let first = LashVmHostIdentities::cell(opener.clone(), "exec-code:1");
        let second = LashVmHostIdentities::cell(opener.clone(), "exec-code:2");

        assert_eq!(
            first.opener(),
            second.opener(),
            "both cells belong to one opener; that is the point"
        );
        assert_ne!(
            first.call_id(0),
            second.call_id(0),
            "two cells of one turn must not mint one leaf identity"
        );
        assert_ne!(
            first.child_call_id(0, 0),
            second.child_call_id(0, 0),
            "two cells of one turn must not mint one child identity"
        );
    }

    /// A cell under a session operation opens on that operation, and two
    /// cells of one operation stay apart: the cell's own replay key is what
    /// keeps them apart (ADR 0099 §1).
    #[test]
    fn two_cells_of_one_session_operation_mint_distinct_identities() {
        let scope = ExecutionScope::session_operation("session-1", "operation-3");
        let opener = EffectOpener::for_scope(&AdmittedScope::new(scope))
            .expect("a session operation is an opener");
        assert_eq!(
            opener,
            EffectOpener::session_operation("session-1", "operation-3")
        );
        let first = LashVmHostIdentities::cell(opener.clone(), "exec-code:1");
        let second = LashVmHostIdentities::cell(opener.clone(), "exec-code:2");

        assert_eq!(
            first.opener(),
            second.opener(),
            "both cells belong to one operation; that is the point"
        );
        assert_ne!(
            first.call_id(0),
            second.call_id(0),
            "two cells of one operation must not mint one leaf identity"
        );
        assert_ne!(
            first.child_call_id(0, 0),
            second.child_call_id(0, 0),
            "two cells of one operation must not mint one child identity"
        );
        assert_ne!(
            opener,
            EffectOpener::for_scope(&AdmittedScope::new(ExecutionScope::turn(
                "session-1",
                "operation-3"
            )),)
            .expect("a turn is an opener"),
            "a session operation is not a turn that happens to spell its id"
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
        let split_early = LashVmHostIdentities::cell(EffectOpener::turn("a:b", "c"), "exec-code:1");
        let split_late = LashVmHostIdentities::cell(EffectOpener::turn("a", "b:c"), "exec-code:1");

        assert_eq!(
            split_early.opener().render(),
            split_late.opener().render(),
            "the diagnostic rendering may stay ambiguous; the identity must not"
        );
        assert_ne!(
            split_early.call_id(0),
            split_late.call_id(0),
            "two splits of `a:b:c` are different openers and must mint different leaves"
        );
        assert_ne!(
            split_early.child_call_id(0, 0),
            split_late.child_call_id(0, 0),
            "two splits of `a:b:c` are different openers and must mint different children"
        );
    }

    /// The session-operation arm has the same collision in its rendering:
    /// `SessionOperation("a:b", "c")` and `SessionOperation("a", "b:c")` both
    /// render the same text.
    #[test]
    fn delimiter_bearing_session_operation_components_mint_distinct_identities() {
        let split_early =
            LashVmHostIdentities::cell(EffectOpener::session_operation("a:b", "c"), "exec-code:1");
        let split_late =
            LashVmHostIdentities::cell(EffectOpener::session_operation("a", "b:c"), "exec-code:1");

        assert_ne!(
            split_early.call_id(0),
            split_late.call_id(0),
            "two splits of `a:b:c` are different operations and must mint different leaves"
        );
        assert_ne!(
            split_early.child_call_id(0, 0),
            split_late.child_call_id(0, 0),
            "two splits of `a:b:c` are different operations and must mint different children"
        );
    }

    /// A session id that itself carries `:` — the shape a spawned child's
    /// session takes (`session:child:{call_id}`) — round-trips through the typed
    /// opener untouched and mints an identity that cannot alias a different
    /// split of the same bytes.
    #[test]
    fn a_delimiter_bearing_session_id_round_trips() {
        let spawned_session = "session:child:lash_vm:turn:1:x:1:y";
        let scope = ExecutionScope::turn(spawned_session, "turn-1");
        let opener =
            EffectOpener::for_scope(&AdmittedScope::new(scope)).expect("a turn is an opener");

        assert_eq!(
            opener.session_id().map(SessionId::as_str),
            Some(spawned_session),
            "the session id round-trips through the typed opener untouched"
        );
        let leaf = LashVmHostIdentities::cell(opener, "exec-code:1").call_id(0);
        assert_ne!(
            leaf,
            LashVmHostIdentities::cell(
                EffectOpener::turn("session:child:lash_vm:turn:1:x:1", "y:turn-1"),
                "exec-code:1",
            )
            .call_id(0),
            "the same bytes split across the session/turn boundary are a different opener"
        );
    }

    /// Two aggregates are separated by the ordinals they were issued at, and
    /// the leaves of one aggregate by their first-appearance index in it.
    #[test]
    fn two_aggregates_mint_four_child_identities() {
        let identities = process_opener("worker");
        let minted = [3u64, 4]
            .into_iter()
            .flat_map(|ordinal| {
                let identities = &identities;
                [0u64, 1]
                    .into_iter()
                    .map(move |leaf_index| identities.child_call_id(ordinal, leaf_index))
            })
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(
            minted.len(),
            4,
            "two leaves of two aggregates are four identities"
        );
    }

    /// No compiler output reaches an identity: the id of a command is a
    /// function of the opener, the cell, and the command's issue ordinal
    /// alone, and the namespace of a run is a function of its cell key.
    #[test]
    fn an_identity_is_the_issue_ordinal_under_the_run_scope() {
        let cell =
            LashVmHostIdentities::cell(EffectOpener::turn("session-1", "turn-7"), "exec-code:1");
        assert_ne!(cell.call_id(0), cell.call_id(1));
        assert_eq!(
            cell.call_id(7),
            lash_core::ToolCallId::derive(
                "",
                lash_core::ToolCallRoot::turn(&cell.opener().identity_encoding())
                    .expect("a turn opener has a handle"),
                &[
                    lash_core::ToolCallPosition::CodeOpener(&cell.opener().identity_encoding()),
                    lash_core::ToolCallPosition::CodeCell("exec-code:1"),
                    lash_core::ToolCallPosition::CodeCommand(7),
                ],
            ),
            "a cell's command is named under its turn root by opener, cell and ordinal"
        );
        assert_eq!(
            cell.namespace().command(7).as_str(),
            "exec-code:1:lk2:0000000007"
        );
        assert_eq!(cell.namespace().seal(), "exec-code:1:lk2:~seal");
    }
}
