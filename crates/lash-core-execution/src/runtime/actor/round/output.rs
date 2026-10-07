//! An attempt's settled output: its outcome and the journal-local payload
//! of the material it names, as one checked value.
//!
//! A completion, a known failure and a park name material, and carry its
//! payload; an interruption, a limit and a cancel name none, and carry none.
//! Only a check builds a payload-bearing output: the payload's bytes hash,
//! through the material codec, to the digest its outcome names. Every
//! stored form (an outcome or retry record, a step's settlement in engine
//! state) decodes through [`SettledOutput`]'s own form and the same check.

use lash_core_store::tool_run::{
    AttemptOutcome, AvailableEvidence, CompletionSource, KnownFailure, KnownFailureReason,
    LimitCause, MaterialDigest, MaterialLocation, MaterialOwner, MaterialPayload, MaterialRef,
    MaterialRefusal, MaterialRole,
};
use serde::{Deserialize, Serialize};

/// What names one material: the reference whose payload rides beside it.
pub trait NamesMaterial {
    /// The reference.
    fn material(&self) -> &MaterialRef;
}

impl NamesMaterial for MaterialRef {
    fn material(&self) -> &MaterialRef {
        self
    }
}

impl NamesMaterial for KnownFailure {
    fn material(&self) -> &MaterialRef {
        &self.output
    }
}

impl NamesMaterial for CompletionSource {
    fn material(&self) -> &MaterialRef {
        &self.metadata
    }
}

/// `T`, which names one material, with that material's journal-local
/// payload, whose bytes hash to the digest `T` names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Material<T = MaterialRef> {
    named: T,
    payload: String,
}

impl<T: NamesMaterial> Material<T> {
    /// `payload` as the material `named` names.
    ///
    /// # Errors
    ///
    /// [`MaterialRefusal::Corrupt`] when the payload's bytes do not hash to
    /// the named digest.
    pub fn new(named: T, payload: String) -> Result<Self, SettledOutputRefusal> {
        let reference = named.material();
        reference.verify(&reference.owner, &digest(reference, &payload))?;
        Ok(Self { named, payload })
    }

    /// What names the material.
    #[must_use]
    pub fn named(&self) -> &T {
        &self.named
    }

    /// The material's reference.
    #[must_use]
    pub fn reference(&self) -> &MaterialRef {
        self.named.material()
    }

    /// The material's payload.
    #[must_use]
    pub fn payload(&self) -> &str {
        &self.payload
    }

    /// What names the material, and its payload.
    #[must_use]
    pub fn into_parts(self) -> (T, String) {
        (self.named, self.payload)
    }
}

impl Material {
    /// `payload` as `owner`'s journal-local material of `role`, named by the
    /// reference the material codec mints for its bytes.
    #[must_use]
    pub fn journal_local(owner: MaterialOwner, role: MaterialRole, payload: String) -> Self {
        let named = mint(owner, role, MaterialLocation::JournalLocal, &payload);
        Self { named, payload }
    }

    /// This material as a known failure's output.
    #[must_use]
    pub fn failure(
        self,
        reason: KnownFailureReason,
        suggested_delay_ms: Option<u64>,
    ) -> Material<KnownFailure> {
        Material {
            named: KnownFailure {
                output: self.named,
                reason,
                suggested_delay_ms,
            },
            payload: self.payload,
        }
    }

    /// This material as a parked call's pending completion, racing the tool
    /// completion wait `wait`, by its id's hex.
    #[must_use]
    pub fn parked(self, wait: String) -> Material<CompletionSource> {
        Material {
            named: CompletionSource {
                wait,
                terminal: None,
                metadata: self.named,
            },
            payload: self.payload,
        }
    }
}

impl Material<CompletionSource> {
    /// The park, also racing the process terminal wait `terminal`, by its
    /// id's hex. The wait is not part of the material.
    pub fn await_terminal(&mut self, terminal: String) {
        self.named.terminal = Some(terminal);
    }
}

fn mint(
    owner: MaterialOwner,
    role: MaterialRole,
    location: MaterialLocation,
    payload: &str,
) -> MaterialRef {
    #[expect(
        clippy::expect_used,
        reason = "a material payload is plain data whose encoding cannot fail"
    )]
    MaterialPayload::new(owner, role, None, payload.to_owned())
        .reference(location)
        .expect("a material payload encodes")
}

/// The digest `payload` hashes to as the material `reference` names.
fn digest(reference: &MaterialRef, payload: &str) -> MaterialDigest {
    mint(
        reference.owner.clone(),
        reference.role,
        reference.location.clone(),
        payload,
    )
    .digest
}

/// How an attempt settled: its outcome and, for an outcome that names
/// material, that material's payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SettledOutputForm", into = "SettledOutputForm")]
pub enum SettledOutput {
    /// It completed with this output.
    Completed(Material),
    /// It reported this known failure.
    Failed(Material<KnownFailure>),
    /// It parked on these waits, with its pending completion.
    Waiting(Material<CompletionSource>),
    /// It started and never settled.
    Interrupted,
    /// It ran past a limit.
    TimedOut {
        /// Which limit.
        cause: LimitCause,
        /// What it retained before it stopped.
        evidence: AvailableEvidence,
    },
    /// It was cancelled.
    Cancelled {
        /// What it retained before it stopped.
        evidence: AvailableEvidence,
    },
}

/// Why an outcome and a payload are not a [`SettledOutput`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SettledOutputRefusal {
    /// A completion, a known failure or a park without its material's
    /// payload.
    #[error("an outcome that names material settled without its payload")]
    MissingPayload,
    /// A payload beside an outcome that names no material.
    #[error("an outcome that names no material settled with a payload")]
    StrayPayload,
    /// A payload whose bytes do not hash to the digest its outcome names.
    #[error(transparent)]
    Material(#[from] MaterialRefusal),
}

impl SettledOutput {
    /// `outcome` with `payload`, the payload of the material it names.
    ///
    /// # Errors
    ///
    /// [`SettledOutputRefusal`] when the payload is missing or stray, or its
    /// bytes do not hash to the outcome's digest.
    pub fn new(
        outcome: AttemptOutcome,
        payload: Option<String>,
    ) -> Result<Self, SettledOutputRefusal> {
        match (outcome, payload) {
            (AttemptOutcome::Completed(output), Some(payload)) => {
                Ok(Self::Completed(Material::new(output, payload)?))
            }
            (AttemptOutcome::Failed(failure), Some(payload)) => {
                Ok(Self::Failed(Material::new(failure, payload)?))
            }
            (AttemptOutcome::Waiting(source), Some(payload)) => {
                Ok(Self::Waiting(Material::new(source, payload)?))
            }
            (
                AttemptOutcome::Completed(_)
                | AttemptOutcome::Failed(_)
                | AttemptOutcome::Waiting(_),
                None,
            ) => Err(SettledOutputRefusal::MissingPayload),
            (_, Some(_)) => Err(SettledOutputRefusal::StrayPayload),
            (AttemptOutcome::Interrupted, None) => Ok(Self::Interrupted),
            (AttemptOutcome::TimedOut { cause, evidence }, None) => {
                Ok(Self::TimedOut { cause, evidence })
            }
            (AttemptOutcome::Cancelled { evidence }, None) => Ok(Self::Cancelled { evidence }),
        }
    }

    /// The attempt's outcome, as its record names it.
    #[must_use]
    pub fn outcome(&self) -> AttemptOutcome {
        match self {
            Self::Completed(output) => AttemptOutcome::Completed(output.named().clone()),
            Self::Failed(failure) => AttemptOutcome::Failed(failure.named().clone()),
            Self::Waiting(source) => AttemptOutcome::Waiting(source.named().clone()),
            Self::Interrupted => AttemptOutcome::Interrupted,
            Self::TimedOut { cause, evidence } => AttemptOutcome::TimedOut {
                cause: *cause,
                evidence: evidence.clone(),
            },
            Self::Cancelled { evidence } => AttemptOutcome::Cancelled {
                evidence: evidence.clone(),
            },
        }
    }

    /// The payload of the material the outcome names.
    #[must_use]
    pub fn payload(&self) -> Option<&str> {
        match self {
            Self::Completed(output) => Some(output.payload()),
            Self::Failed(failure) => Some(failure.payload()),
            Self::Waiting(source) => Some(source.payload()),
            Self::Interrupted | Self::TimedOut { .. } | Self::Cancelled { .. } => None,
        }
    }

    /// The outcome and its payload.
    #[must_use]
    pub fn into_parts(self) -> (AttemptOutcome, Option<String>) {
        let outcome = self.outcome();
        let payload = match self {
            Self::Completed(output) => Some(output.into_parts().1),
            Self::Failed(failure) => Some(failure.into_parts().1),
            Self::Waiting(source) => Some(source.into_parts().1),
            Self::Interrupted | Self::TimedOut { .. } | Self::Cancelled { .. } => None,
        };
        (outcome, payload)
    }

    /// Whether a pinned `Repeatable` contract may repeat the attempt: a
    /// known failure or a slice expiry.
    #[must_use]
    pub fn may_repeat(&self) -> bool {
        matches!(
            self,
            Self::Failed(_)
                | Self::TimedOut {
                    cause: LimitCause::ExecutionSlice,
                    ..
                }
        )
    }

    /// The answer of an output that names no material: what a crash, a
    /// limit or a cancel left of the call. Every reader of a settled call,
    /// a turn's presentation and a process engine's injection alike, answers
    /// these from here. `None` for an output whose payload is its answer.
    #[must_use]
    pub fn stopped_answer(&self) -> Option<crate::ToolCallOutput> {
        Some(match self {
            Self::Interrupted => crate::ToolCallOutput::failure(
                crate::ToolFailure::runtime(
                    crate::ToolFailureClass::Execution,
                    "tool_interrupted",
                    "tool was interrupted by a runtime restart; it may or may not have taken effect, and may still be running.",
                )
                .with_cause(crate::ToolFailureCause::Interrupted),
            ),
            Self::TimedOut { cause, .. } => crate::ToolCallOutput::failure(
                crate::ToolFailure::runtime(
                    crate::ToolFailureClass::Timeout,
                    "tool_timed_out",
                    format!(
                        "tool exceeded its {cause:?} limit; it may have partly run, and may still be running."
                    ),
                )
                .with_cause(crate::ToolFailureCause::ExecutionLimit { cause: *cause }),
            ),
            Self::Cancelled { .. } => crate::ToolCallOutput::cancelled(
                crate::ToolCancellation::runtime("the call was cancelled"),
            ),
            Self::Completed(_) | Self::Failed(_) | Self::Waiting(_) => return None,
        })
    }
}

/// The stored form of a [`SettledOutput`]: the outcome as its record names
/// it, and the payload of its material. It decodes only through the check.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettledOutputForm {
    outcome: AttemptOutcome,
    payload: Option<String>,
}

impl TryFrom<SettledOutputForm> for SettledOutput {
    type Error = SettledOutputRefusal;

    fn try_from(form: SettledOutputForm) -> Result<Self, Self::Error> {
        Self::new(form.outcome, form.payload)
    }
}

impl From<SettledOutput> for SettledOutputForm {
    fn from(output: SettledOutput) -> Self {
        let (outcome, payload) = output.into_parts();
        Self { outcome, payload }
    }
}

#[cfg(test)]
mod tests {
    use lash_core_store::effect_opener::EffectOpener;
    use lash_core_store::tool_run::{MaterialOwner, MaterialRole};

    use super::{Material, SettledOutput};

    /// A settled output decodes only with the payload its outcome names: a
    /// missing payload, bytes that do not hash to the outcome's digest, or a
    /// payload beside an outcome that names none is refused at decode.
    #[test]
    fn a_settled_output_decodes_only_with_the_payload_its_outcome_names() {
        let valid = SettledOutput::Completed(Material::journal_local(
            MaterialOwner::Run {
                opener: EffectOpener::turn("s", "t"),
            },
            MaterialRole::AttemptOutput,
            "alpha".to_owned(),
        ));
        let json = serde_json::to_string(&valid).unwrap();
        assert_eq!(serde_json::from_str::<SettledOutput>(&json).unwrap(), valid);
        for forged in [
            json.replace("\"alpha\"", "\"omega\""),
            json.replace("\"alpha\"", "null"),
            r#"{"outcome":{"result":"interrupted"},"payload":"alpha"}"#.to_owned(),
        ] {
            assert!(
                serde_json::from_str::<SettledOutput>(&forged).is_err(),
                "{forged} is refused"
            );
        }
    }
}
