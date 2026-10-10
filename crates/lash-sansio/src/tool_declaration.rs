//! The author-facing tool declaration (K1, binding Q3).
//!
//! A tool declares exactly four capabilities on its [`ToolManifest`]:
//! whether its body may return Deferred, which Lash intent kinds a Done
//! result may declare, whether the call is isolated — a process from its
//! start, with no inline body — and which turn-ending controls its result
//! may be ([`TurnControls`]). A manifest holds only a valid declaration
//! ([`ToolManifest::declared`](crate::ToolManifest::declared)): an invalid
//! one, or an isolated one naming no process engine, is refused when its
//! tool is registered, so no call is refused for it. Admission records the
//! declaration with the admitted manifest, and every later read — the completion key reserved
//! before the body, the check of the body's outcome, a recovered or replayed
//! call — reads that recorded answer, never a live provider.
//!
//! There is no per-call timeout, duration, budget or idempotent capability:
//! the declaration refuses those fields when decoding. Crash recovery is
//! at-least-once under the stable [`ToolCallId`](crate::ToolCallId) and
//! attempt ordinal, which is the external idempotency key. Retry and cancel
//! policy are runtime policy, and [`ToolManifest::inline`] stays catalog
//! policy; neither is part of this declaration.
//!
//! [`ToolManifest`]: crate::ToolManifest
//! [`ToolManifest::inline`]: crate::ToolManifest::inline

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::ToolIntentKind;

/// A way a tool's call may end its caller's turn.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TurnControlKind {
    /// The turn ends with the value the call carries.
    Finish,
    /// The turn ends by switching to a fresh agent frame.
    SwitchAgentFrame,
}

impl TurnControlKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Finish => "finish",
            Self::SwitchAgentFrame => "switch_agent_frame",
        }
    }
}

/// The turn-ending controls a tool declares its result may be. A tool that
/// declares any has no output: a control is its call's whole result.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct TurnControls(BTreeSet<TurnControlKind>);

impl TurnControls {
    /// No control: an ordinary tool.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn finish() -> Self {
        Self::none().with(TurnControlKind::Finish)
    }

    #[must_use]
    pub fn switch_agent_frame() -> Self {
        Self::none().with(TurnControlKind::SwitchAgentFrame)
    }

    #[must_use]
    pub fn with(mut self, kind: TurnControlKind) -> Self {
        self.0.insert(kind);
        self
    }

    pub fn contains(&self, kind: TurnControlKind) -> bool {
        self.0.contains(&kind)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = TurnControlKind> + '_ {
        self.0.iter().copied()
    }
}

/// The author-facing tool declaration: exactly four capabilities.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolDeclaration {
    /// The body may return Deferred and park on a source.
    pub may_defer: bool,
    /// The intent kinds a Done result may declare, in vocabulary order
    /// without duplicates.
    pub intents: Vec<ToolIntentKind>,
    /// The call runs as a process from its start, with no inline body. It is
    /// not spelled as an intent plus Deferred.
    pub isolated: bool,
    /// The turn-ending controls the call's result may be. A call to a tool
    /// that declares any is admitted only where it can end the turn, and a
    /// result carrying a control the tool did not declare is refused.
    pub controls: TurnControls,
}

/// The shape of a body's outcome, as a declaration checks it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutcomeShape<'a> {
    /// A Done result declaring `intents`, which is the turn control
    /// `control` when it is one.
    Done {
        intents: &'a [ToolIntentKind],
        control: Option<TurnControlKind>,
    },
    /// A Deferred result parked on a source.
    Deferred,
}

/// Why a declaration, or an outcome under it, is refused.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error, schemars::JsonSchema,
)]
#[serde(tag = "refusal", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeclarationRefusal {
    /// A deferring body returned a key other than the source its Run armed.
    #[error("a Deferred attempt returned a source its Run did not arm")]
    UnarmedSource,
    #[error("intent kind `{}` is declared twice", kind.as_str())]
    DuplicateIntent { kind: ToolIntentKind },
    #[error("declared intents are not in vocabulary order")]
    IntentOrder,
    /// An isolated call has no inline body, so it cannot declare what only
    /// an inline body returns.
    #[error("an isolated call declares an inline capability")]
    IsolatedInlineCapability,
    #[error("an inline body returned Deferred without declaring `may_defer`")]
    UndeclaredDeferral,
    #[error("a body declared intent kind `{}` it did not declare", kind.as_str())]
    UndeclaredIntent { kind: ToolIntentKind },
    #[error("a body's result is the turn control `{}`, which it did not declare", control.as_str())]
    UndeclaredControl { control: TurnControlKind },
    #[error("an isolated call produced an inline outcome")]
    InlineOutcomeFromIsolated,
}

fn intent_position(kind: ToolIntentKind) -> usize {
    ToolIntentKind::ALL
        .iter()
        .position(|candidate| *candidate == kind)
        .unwrap_or(usize::MAX)
}

impl ToolDeclaration {
    /// A body that may return Deferred and park on a source.
    #[must_use]
    pub fn deferring() -> Self {
        Self {
            may_defer: true,
            ..Self::default()
        }
    }

    /// A Done result of this body may declare `intents`. The kinds are kept
    /// in vocabulary order without duplicates, whatever order they come in.
    #[must_use]
    pub fn with_intents(mut self, intents: impl IntoIterator<Item = ToolIntentKind>) -> Self {
        let kinds: BTreeSet<usize> = self
            .intents
            .iter()
            .copied()
            .chain(intents)
            .map(intent_position)
            .collect();
        self.intents = kinds
            .into_iter()
            .filter_map(|position| ToolIntentKind::ALL.get(position).copied())
            .collect();
        self
    }

    /// This declaration, also declaring `controls`.
    #[must_use]
    pub fn with_controls(mut self, controls: TurnControls) -> Self {
        self.controls = controls;
        self
    }

    /// Check the declaration itself.
    ///
    /// # Errors
    ///
    /// The first [`DeclarationRefusal`] found.
    pub fn validate(&self) -> Result<(), DeclarationRefusal> {
        let mut seen = BTreeSet::new();
        for kind in &self.intents {
            if !seen.insert(intent_position(*kind)) {
                return Err(DeclarationRefusal::DuplicateIntent { kind: *kind });
            }
        }
        if !self
            .intents
            .windows(2)
            .all(|pair| intent_position(pair[0]) < intent_position(pair[1]))
        {
            return Err(DeclarationRefusal::IntentOrder);
        }
        if self.isolated && (self.may_defer || !self.intents.is_empty()) {
            return Err(DeclarationRefusal::IsolatedInlineCapability);
        }
        Ok(())
    }

    /// Check a body's outcome against the declaration, before anything the
    /// outcome declares is realized.
    ///
    /// # Errors
    ///
    /// An undeclared Deferred, intent or turn control, or any inline
    /// outcome of an isolated call.
    pub fn admits(&self, outcome: OutcomeShape<'_>) -> Result<(), DeclarationRefusal> {
        if self.isolated {
            return Err(DeclarationRefusal::InlineOutcomeFromIsolated);
        }
        match outcome {
            OutcomeShape::Deferred if !self.may_defer => {
                Err(DeclarationRefusal::UndeclaredDeferral)
            }
            OutcomeShape::Deferred => Ok(()),
            OutcomeShape::Done { intents, control } => {
                if let Some(kind) = intents.iter().find(|kind| !self.intents.contains(kind)) {
                    return Err(DeclarationRefusal::UndeclaredIntent { kind: *kind });
                }
                match control {
                    Some(control) if !self.controls.contains(control) => {
                        Err(DeclarationRefusal::UndeclaredControl { control })
                    }
                    _ => Ok(()),
                }
            }
        }
    }

    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_intents_keeps_vocabulary_order_without_duplicates() {
        let declaration = ToolDeclaration::default().with_intents([
            ToolIntentKind::CancelProcess,
            ToolIntentKind::StartProcess,
            ToolIntentKind::CancelProcess,
        ]);
        assert_eq!(
            declaration.intents,
            vec![ToolIntentKind::StartProcess, ToolIntentKind::CancelProcess]
        );
        declaration
            .validate()
            .expect("a built declaration is valid");
    }
}
