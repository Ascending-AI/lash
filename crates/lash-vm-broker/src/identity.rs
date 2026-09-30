//! The one derivation of the call identities model code's commands take
//! (ADR 0117 §2).
//!
//! The parent derives every identity: a command is named by the issue ordinal
//! the parent gave it when it admitted the command, under the opener that
//! admitted the run, never by anything the worker sent. Both Lashlang hosts
//! and the broker mint from here, so there is one spelling of a code
//! command's `ToolCallId`.

use lash_core_store::effect_opener::EffectOpener;
use lash_sansio::{ToolCallId, ToolCallPosition};

/// The identities one run of model code mints.
///
/// Two facts, and only one of them is the opener. [`EffectOpener`] is the
/// lifecycle owner (ADR 0099 §1): a turn, or one process. `execution` is the
/// part of the identity the opener is deliberately too coarse to supply: a
/// turn runs many cells, and two cells of one turn each count their ordinals
/// from zero, so without the cell's own key they would mint the same ids. A
/// process body has no such subdivision: it is one run for its whole life,
/// across every segment, so it carries none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeCallIdentities {
    opener: EffectOpener,
    execution: Option<String>,
}

impl CodeCallIdentities {
    /// The identities one cell of a turn mints; `execution_key` is the cell's
    /// own replay key inside the turn.
    pub fn cell(opener: EffectOpener, execution_key: impl Into<String>) -> Self {
        Self {
            opener,
            execution: Some(execution_key.into()),
        }
    }

    /// The identities one process body mints, for the whole life of the
    /// process.
    pub fn process_body(process_id: lash_sansio::ProcessId) -> Self {
        Self {
            opener: EffectOpener::process(process_id),
            execution: None,
        }
    }

    /// The opener every identity binds.
    pub fn opener(&self) -> &EffectOpener {
        &self.opener
    }

    /// The cell's replay key inside its turn; `None` for a process body.
    pub fn execution(&self) -> Option<&str> {
        self.execution.as_deref()
    }

    /// The opener scope, canonically encoded: every component is
    /// length-prefixed, so two different `(opener, execution)` pairs never
    /// encode alike.
    pub fn scope(&self) -> String {
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

    /// The id of the call the program issued at `ordinal`: a cell's call at
    /// `[code opener, cell, command]`, a process body's at
    /// `[code opener, command]`.
    pub fn call_id(&self, ordinal: u64) -> ToolCallId {
        self.derive(ordinal, None)
    }

    /// The id of the leaf at `leaf_index` (its first-appearance index in the
    /// aggregate as written, not the order it settled in) of the aggregate the
    /// program issued at `ordinal`.
    pub fn child_call_id(&self, ordinal: u64, leaf_index: u64) -> ToolCallId {
        self.derive(ordinal, Some(leaf_index))
    }

    fn derive(&self, ordinal: u64, leaf_index: Option<u64>) -> ToolCallId {
        let opener = self.opener.identity_encoding();
        let mut positions = vec![ToolCallPosition::CodeOpener(&opener)];
        if let Some(execution) = &self.execution {
            positions.push(ToolCallPosition::CodeCell(execution));
        }
        positions.push(ToolCallPosition::CodeCommand(ordinal));
        if let Some(leaf_index) = leaf_index {
            positions.push(ToolCallPosition::CodeAggregate(leaf_index));
        }
        self.opener.tool_call_admission().call_id(&positions)
    }
}
