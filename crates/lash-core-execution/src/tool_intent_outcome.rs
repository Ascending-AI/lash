use crate::{ToolIntentIdentity, ToolIntentKind};
use serde::{Deserialize, Serialize};

macro_rules! realized_payload {
    (StartProcess) => { crate::ProcessHandleView };
    (SignalProcess) => { Box<crate::ProcessSignal> };
    (CancelProcess) => { crate::ProcessCancelReceipt };
    (EmitProcessEvent) => { Box<crate::ProcessEvent> };
    (EmitTrigger) => { crate::facade_support::TriggerEmitReport };
    (GetDefinition) => { Box<crate::ProcessDefinition> };
    (PublishDefinition) => { Box<crate::ProcessDefinition> };
    (RegisterTrigger) => { Box<crate::TriggerMutationReceipt> };
}

macro_rules! realized_model_value {
    (RegisterTrigger, $result:expr) => {
        crate::trigger_handle_outcome_value($result)
            .map_err(|error| <serde_json::Error as serde::ser::Error>::custom(error))
    };
    ($variant:ident, $result:expr) => {
        serde_json::to_value($result)
    };
}

macro_rules! define_realized {
    ($($variant:ident $wire:literal,)*) => {
        /// The complete result of one realized intent. Its kind is derived.
        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
        #[serde(tag = "kind", content = "result", rename_all = "snake_case", deny_unknown_fields)]
        pub enum ToolIntentRealized { $($variant(realized_payload!($variant)),)* }
        impl ToolIntentRealized {
            pub fn kind(&self) -> ToolIntentKind {
                match self { $(Self::$variant(_) => ToolIntentKind::$variant,)* }
            }
            /// Serialize only at the model presentation boundary.
            pub fn model_value(&self) -> Result<serde_json::Value, serde_json::Error> {
                match self {
                    $(Self::$variant(result) => realized_model_value!($variant, result),)*
                }
            }
        }
    };
}
lash_sansio::tool_intent_variants!(define_realized);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum ToolIntentRefusalReason {
    UnsupportedProtocolVersion {
        recorded: u16,
    },
    IntentIndexOverflow,
    /// A session-turn intent declares no captured environment. Admission
    /// refuses it before recording a start; retrying cannot supply that fact.
    ExecutionEnvMissing,
    CountBudgetExceeded {
        actual: usize,
        maximum: usize,
    },
    CanonicalByteBudgetExceeded {
        actual: usize,
        maximum: usize,
    },
    PerKindBudgetExceeded {
        kind: ToolIntentKind,
        actual: usize,
        maximum: usize,
    },
    OwnerMismatch {
        expected: crate::RuntimeOwner,
        recorded: crate::RuntimeOwner,
    },
    ForeignTriggerOwnerScope {
        expected: crate::TriggerOwnerScope,
        recorded: crate::TriggerOwnerScope,
    },
    ForeignTriggerActor {
        expected: crate::ProcessOriginator,
        recorded: crate::ProcessOriginator,
    },
    CommandFailed {
        cause: crate::ToolIntentCommandFailure,
    },
    /// The logical Run whose emission minted this intent is cancel-decided:
    /// ADR 0099 §4 forbids new semantic admission under a cancelled
    /// invocation, so the intent is refused before any of its commands run.
    MintingRunCancelled,
    /// A declared start whose identity is not the one its call's declaring
    /// attempt derives for index 0 (ADR 0116 §3.1): another call's, another
    /// execution scope's or minting emission's, a nonzero index, or a replay
    /// key its own fields do not derive. `expected` is the identity the
    /// runtime derived from the admitted call; `recorded` is the one the
    /// declaration carried. Nothing was registered.
    DeclaredStartIdentityMismatch {
        expected: Box<ToolIntentIdentity>,
        recorded: Box<ToolIntentIdentity>,
    },
}

impl ToolIntentRefusalReason {
    pub fn code(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::UnsupportedProtocolVersion { .. } => "unsupported_protocol_version".into(),
            Self::IntentIndexOverflow => "intent_index_overflow".into(),
            Self::ExecutionEnvMissing => "execution_env_missing".into(),
            Self::CountBudgetExceeded { .. } => "count_budget_exceeded".into(),
            Self::CanonicalByteBudgetExceeded { .. } => "canonical_byte_budget_exceeded".into(),
            Self::PerKindBudgetExceeded { .. } => "per_kind_budget_exceeded".into(),
            Self::OwnerMismatch { .. } => "owner_mismatch".into(),
            Self::ForeignTriggerOwnerScope { .. } => "foreign_trigger_owner_scope".into(),
            Self::ForeignTriggerActor { .. } => "foreign_trigger_actor".into(),
            Self::CommandFailed { cause } => cause.code(),
            Self::MintingRunCancelled => "minting_run_cancelled".into(),
            Self::DeclaredStartIdentityMismatch { .. } => "declared_start_identity_mismatch".into(),
        }
    }

    /// The refusal as the model reads it: its code and the facts that
    /// decided it, so a declaration's failed call names why nothing was
    /// realized (FIG-4255).
    pub fn describe(&self) -> String {
        let code = self.code();
        match self {
            Self::UnsupportedProtocolVersion { recorded } => {
                format!("{code}: intent protocol version {recorded} is not this build's")
            }
            Self::IntentIndexOverflow => format!("{code}: the intent index does not fit in u32"),
            Self::ExecutionEnvMissing => {
                format!("{code}: a session-turn intent requires a captured execution env")
            }
            Self::CountBudgetExceeded { actual, maximum } => {
                format!(
                    "{code}: the attempt declared {actual} intents; at most {maximum} are admitted"
                )
            }
            Self::CanonicalByteBudgetExceeded { actual, maximum } => format!(
                "{code}: the attempt declared {actual} canonical bytes of intents; at most {maximum} are admitted"
            ),
            Self::PerKindBudgetExceeded {
                kind,
                actual,
                maximum,
            } => format!(
                "{code}: the attempt declared {actual} {} intents; at most {maximum} are admitted",
                kind.as_str()
            ),
            Self::OwnerMismatch { expected, recorded } => format!(
                "{code}: the intent names owner `{recorded}`; the attempt ran under owner `{expected}`"
            ),
            Self::ForeignTriggerOwnerScope { expected, recorded } => format!(
                "{code}: the registration names owner scope {recorded:?}; the attempt resolves {expected:?}"
            ),
            Self::ForeignTriggerActor { expected, recorded } => format!(
                "{code}: the registration names actor {recorded:?}; the attempt resolves {expected:?}"
            ),
            Self::CommandFailed { cause } => format!("{code}: {cause}"),
            Self::MintingRunCancelled => {
                format!("{code}: the logical Run whose emission minted the intent was cancelled")
            }
            Self::DeclaredStartIdentityMismatch { expected, recorded } => format!(
                "{code}: the start was declared as `{}`; its call derives `{}`",
                recorded.replay_key, expected.replay_key
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolIntentExecutionOutcome {
    Executed {
        identity: ToolIntentIdentity,
        realized: ToolIntentRealized,
    },
    Refused {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity: Option<ToolIntentIdentity>,
        intent_index: u32,
        kind: ToolIntentKind,
        refusal: ToolIntentRefusalReason,
    },
    /// Batch-level protocol refusal used when the recorded batch contains no
    /// declarations to which the refusal could honestly be attached.
    ProtocolRefused { refusal: ToolIntentRefusalReason },
}

impl ToolIntentExecutionOutcome {
    pub fn kind(&self) -> Option<ToolIntentKind> {
        match self {
            Self::Executed { realized, .. } => Some(realized.kind()),
            Self::Refused { kind, .. } => Some(*kind),
            Self::ProtocolRefused { .. } => None,
        }
    }

    pub fn model_addendum(&self) -> String {
        match self {
            Self::Executed {
                identity, realized, ..
            } => format!(
                "[tool intent {} #{} executed: {}]",
                realized.kind().as_str(),
                identity.intent_index,
                match realized.model_value() {
                    Ok(value) => value.to_string(),
                    Err(error) => format!("presentation failed: {error}"),
                }
            ),
            Self::Refused {
                intent_index,
                kind,
                refusal,
                ..
            } => format!(
                "[tool intent {} #{} refused: {}]",
                kind.as_str(),
                intent_index,
                refusal.code()
            ),
            Self::ProtocolRefused { refusal } => {
                format!("[tool intent batch refused: {}]", refusal.code())
            }
        }
    }
}
