//! A session actor's claim obligations (S7, FIG-6074): the `requires` its
//! commits write, derived from the work it recorded and has not settled.
//!
//! Every commit of a claimed session passes through [`SessionObligations`].
//! The one derivation is [`SessionObligations::stage`]: the transaction's
//! appended run facts are read, an admission holds each member's bound
//! callbacks under its call and an outcome settles the call. `requires` is
//! the union of every held call's tokens, rewritten in the same transaction
//! only when it changes. A park, a wait or a retry keeps its call held, and
//! one settled call of two to the same provider leaves the provider
//! required.
//!
//! An activation starts from what the actor carried when it was claimed:
//! those tokens stay required until its turn ends, as its records may still
//! name them. A turn's commit, cancel or close derives them again: an ended
//! turn holds nothing.

use std::collections::{BTreeMap, BTreeSet};

use lash_core_store::obligations::{ObligationToken, RegistryKind};
use lash_durable::domain::graph::{RunFactKind, RunFactWrite};
use lash_durable::{ActorTx, CommitLabel, DomainWrite, Epoch};
use lash_sansio::sync::MutexExt as _;

use crate::ToolCallId;

/// The tokens `binding`'s callbacks are required under in a session.
#[must_use]
pub fn binding_tokens(binding: &crate::tool_run::AdmittedBinding) -> Vec<String> {
    ObligationToken::of_callbacks(RegistryKind::Session, binding.callbacks())
}

/// A process's `requires` (S7): its env's component config tokens, and the
/// tokens of every binding its unsettled steps were admitted under, as
/// process callbacks. Sorted, without duplicates.
#[must_use]
pub fn process_requires<'a>(
    components: impl IntoIterator<Item = String>,
    unsettled: impl IntoIterator<Item = &'a crate::tool_run::AdmittedBinding>,
) -> Vec<String> {
    let mut requires: BTreeSet<String> = components.into_iter().collect();
    for binding in unsettled {
        requires.extend(ObligationToken::of_callbacks(
            RegistryKind::Process,
            binding.callbacks(),
        ));
    }
    requires.into_iter().collect()
}

/// The ledger one claim of a session commits its obligations through.
#[derive(Debug)]
pub(super) struct SessionObligations {
    state: std::sync::Mutex<State>,
}

#[derive(Debug)]
struct State {
    /// What the actor carried when claimed, until its turn ends.
    carried: BTreeSet<String>,
    /// Each unsettled call's tokens, by call: admitted, or staged by a
    /// commit in flight.
    held: BTreeMap<ToolCallId, Vec<String>>,
    /// What the last staged commit wrote, or the claim read; `None` once a
    /// commit failed and the stored value is unknown.
    written: Option<BTreeSet<String>>,
}

/// What one commit changes: the calls it admits and settles, whether it
/// ends the turn, or the requirements its writer set itself.
#[derive(Debug, Default)]
pub(super) struct Staged {
    admitted: Vec<ToolCallId>,
    settled: Vec<ToolCallId>,
    turn_ended: bool,
    explicit: Option<BTreeSet<String>>,
}

impl SessionObligations {
    /// The ledger of a claim of an actor that carried `requires`.
    pub(super) fn claimed(requires: &[String]) -> Self {
        let carried: BTreeSet<String> = requires.iter().cloned().collect();
        Self {
            state: std::sync::Mutex::new(State {
                written: Some(carried.clone()),
                carried,
                held: BTreeMap::new(),
            }),
        }
    }

    /// Derive `tx`'s requirements, committed under `label` at `epoch`, and
    /// write them into it when they change. A transaction whose writer set
    /// its requirements keeps them; the ledger takes them as written.
    pub(super) fn stage(&self, tx: &mut ActorTx, label: CommitLabel, epoch: Epoch) -> Staged {
        let mut state = self.state.lock_recover();
        if let Some(explicit) = tx.requires() {
            let explicit: BTreeSet<String> = explicit.iter().cloned().collect();
            state.written = Some(explicit.clone());
            return Staged {
                explicit: Some(explicit),
                ..Staged::default()
            };
        }
        let mut staged = Staged {
            turn_ended: label == CommitLabel::TURN_COMMIT,
            ..Staged::default()
        };
        let mut admits = false;
        for write in tx.domain() {
            if let DomainWrite::RunFact(RunFactWrite::Append { kind, call, .. }) = write {
                match (kind, call) {
                    (RunFactKind::Admit, _) => admits = true,
                    (RunFactKind::XOutcome, Some(call)) => staged.settled.push(call.clone()),
                    _ => {}
                }
            }
        }
        if admits {
            let facts =
                lash_durable::domain::graph::RunFacts::appended(tx.domain(), tx.graph(), epoch);
            for (call, binding) in super::round::admitted_bindings(&facts) {
                let tokens = binding.as_ref().map(binding_tokens).unwrap_or_default();
                state.held.insert(call.clone(), tokens);
                staged.admitted.push(call);
            }
        }
        if staged.admitted.is_empty() && staged.settled.is_empty() && !staged.turn_ended {
            return staged;
        }
        let mut required = if staged.turn_ended {
            BTreeSet::new()
        } else {
            state.carried.clone()
        };
        for (call, tokens) in &state.held {
            if !staged.settled.contains(call) {
                required.extend(tokens.iter().cloned());
            }
        }
        if state.written.as_ref() != Some(&required) {
            tx.set_requires(required.iter().cloned().collect());
            state.written = Some(required);
        }
        staged
    }

    /// Settle what `staged` changed once its commit's fate is known:
    /// `committed` when the store acknowledged it.
    pub(super) fn concluded(&self, staged: Staged, committed: bool) {
        let mut state = self.state.lock_recover();
        if !committed {
            for call in &staged.admitted {
                state.held.remove(call);
            }
            state.written = None;
            return;
        }
        for call in &staged.settled {
            state.held.remove(call);
        }
        if staged.turn_ended {
            state.carried.clear();
        }
        if let Some(explicit) = staged.explicit {
            if explicit.is_empty() {
                state.carried.clear();
                state.held.clear();
            }
            state.written = Some(explicit);
        }
    }
}
