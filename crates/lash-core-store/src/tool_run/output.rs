//! An attempt's settled output: its outcome and the journal-local payload
//! of the material it names, as one checked value.
//!
//! A completion, a known failure and a park name material, and carry its
//! payload; an interruption, a limit and a cancel name none, and carry none.
//! Only a check builds a payload-bearing output: the payload's bytes hash,
//! through the material codec, to the digest its outcome names. Every
//! stored form (an outcome or retry record, a step's settlement in engine
//! state) decodes through [`SettledOutput`]'s own form and the same check.

use std::any::Any;
use std::fmt;
use std::sync::{Arc, OnceLock};

use super::AttemptOutcome;
use super::AvailableEvidence;
use super::CompletionSource;
use super::KnownFailure;
use super::KnownFailureReason;
use super::MaterialDigest;
use super::MaterialLocation;
use super::MaterialOwner;
use super::MaterialRef;
use super::MaterialRefusal;
use super::MaterialRole;
use super::material::{text_reference, value_reference};
use lash_vocabulary::tool_contract::LimitCause;
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
///
/// The payload is its exact text, or the typed value whose canonical JSON is
/// that text: a producer that holds its answer typed keeps it typed, and its
/// digest is minted over the JSON streamed from the value. A reader asks for
/// the value it expects ([`typed`](Self::typed)); a reader of text gets it
/// projected once, on first ask ([`payload`](Self::payload)).
#[derive(Clone, Debug)]
pub struct Material<T = MaterialRef> {
    named: T,
    // Checked with `named` once; cloning a material never copies or rehashes
    // its immutable payload. String preserves ownership when into_parts is unique.
    payload: Arc<Payload>,
}

/// A material's payload: its text, or the value whose canonical JSON it is.
#[derive(Debug)]
enum Payload {
    Text(String),
    Typed {
        value: Box<dyn TypedPayload>,
        /// The value's canonical JSON, projected for a reader of text.
        text: OnceLock<String>,
    },
}

/// A value a material may hold typed: its canonical JSON is the material's
/// text, the one spelling its digest names.
trait TypedPayload: Any + fmt::Debug + Send + Sync {
    /// Writes the value's canonical JSON to `out`.
    ///
    /// # Errors
    ///
    /// The value does not encode.
    fn write_json(&self, out: &mut dyn std::io::Write) -> serde_json::Result<()>;

    /// The value, to downcast.
    fn as_any(&self) -> &dyn Any;
}

impl<V: Serialize + Any + fmt::Debug + Send + Sync> TypedPayload for V {
    fn write_json(&self, out: &mut dyn std::io::Write) -> serde_json::Result<()> {
        serde_json::to_writer(out, self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Two materials are equal when they are named alike: a material's payload
/// always hashes to the digest it is named by, so equal names name equal
/// bytes, whether a payload is held as text or typed.
impl<T: PartialEq> PartialEq for Material<T> {
    fn eq(&self, other: &Self) -> bool {
        self.named == other.named
    }
}

impl<T: Eq> Eq for Material<T> {}

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
        Ok(Self {
            named,
            payload: Arc::new(Payload::Text(payload)),
        })
    }

    /// `value` as the material `named` names: its canonical JSON is the
    /// payload, streamed into the digest check without being held as text.
    ///
    /// # Errors
    ///
    /// [`MaterialRefusal::WrongOwner`], then [`MaterialRefusal::Corrupt`]
    /// when the value's JSON does not hash to the named digest or does not
    /// encode.
    pub fn verified<V: Serialize + Any + fmt::Debug + Send + Sync>(
        named: T,
        value: V,
    ) -> Result<Self, SettledOutputRefusal> {
        let reference = named.material();
        let found = value_reference(
            &reference.owner,
            reference.role,
            reference.location.clone(),
            &value,
        )
        .map_err(|_| SettledOutputRefusal::Unencodable)?;
        reference.verify(&reference.owner, &found.digest)?;
        Ok(Self {
            named,
            payload: Arc::new(Payload::typed(value)),
        })
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

    /// The material's payload text: for a payload held typed, its value's
    /// canonical JSON, projected on the first ask and kept.
    #[must_use]
    pub fn payload(&self) -> &str {
        self.payload.text()
    }

    /// The payload's value, when the material holds it typed as a `V`.
    #[must_use]
    pub fn typed<V: Any>(&self) -> Option<&V> {
        match &*self.payload {
            Payload::Typed { value, .. } => value.as_any().downcast_ref(),
            Payload::Text(_) => None,
        }
    }

    /// Whether the material holds its payload as text, not typed.
    #[must_use]
    pub fn holds_text(&self) -> bool {
        matches!(&*self.payload, Payload::Text(_))
    }

    /// What names the material, and its payload text.
    #[must_use]
    pub fn into_parts(self) -> (T, String) {
        let text = match Arc::try_unwrap(self.payload) {
            Ok(Payload::Text(text)) => text,
            Ok(typed) => typed.text().to_owned(),
            Err(shared) => shared.text().to_owned(),
        };
        (self.named, text)
    }
}

impl Payload {
    fn typed<V: TypedPayload>(value: V) -> Self {
        Self::Typed {
            value: Box::new(value),
            text: OnceLock::new(),
        }
    }

    fn text(&self) -> &str {
        match self {
            Self::Text(text) => text,
            Self::Typed { value, text } => text.get_or_init(|| {
                let mut json = Vec::new();
                #[expect(
                    clippy::expect_used,
                    reason = "a typed payload's JSON encoded when its digest was minted, \
                              and serde_json writes UTF-8"
                )]
                {
                    value
                        .write_json(&mut json)
                        .expect("a typed payload encodes");
                    String::from_utf8(json).expect("JSON is UTF-8")
                }
            }),
        }
    }
}

impl Material {
    /// `payload` as `owner`'s journal-local material of `role`, named by the
    /// reference the material codec mints for its bytes.
    #[must_use]
    pub fn journal_local(owner: MaterialOwner, role: MaterialRole, payload: String) -> Self {
        let named = mint(&owner, role, MaterialLocation::JournalLocal, &payload);
        Self {
            named,
            payload: Arc::new(Payload::Text(payload)),
        }
    }

    /// `value` as `owner`'s journal-local material of `role`, held typed:
    /// named by the reference the material codec mints for its canonical
    /// JSON, streamed from the value. The one way a producer seals a typed
    /// answer with its identity.
    ///
    /// # Errors
    ///
    /// [`RecordEncodingFailed`](crate::RuntimeErrorCode::RecordEncodingFailed)
    /// when the value does not encode.
    pub fn journal_local_typed<V: Serialize + Any + fmt::Debug + Send + Sync>(
        owner: MaterialOwner,
        role: MaterialRole,
        value: V,
    ) -> Result<Self, crate::runtime_error::RuntimeEffectControllerError> {
        let named = value_reference(&owner, role, MaterialLocation::JournalLocal, &value)?;
        Ok(Self {
            named,
            payload: Arc::new(Payload::typed(value)),
        })
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
                timer: None,
                metadata: self.named,
            },
            payload: self.payload,
        }
    }
}

impl Material<CompletionSource> {
    pub fn await_timer(&mut self, timer: super::CompletionTimer) {
        self.named.timer = Some(timer);
    }

    /// The park, also racing the process terminal wait `terminal`, by its
    /// id's hex. The wait is not part of the material.
    pub fn await_terminal(&mut self, terminal: String) {
        self.named.terminal = Some(terminal);
    }
}

fn mint(
    owner: &MaterialOwner,
    role: MaterialRole,
    location: MaterialLocation,
    payload: &str,
) -> MaterialRef {
    #[expect(
        clippy::expect_used,
        reason = "a material payload is plain data whose encoding cannot fail"
    )]
    text_reference(owner, role, location, payload).expect("a material payload encodes")
}

/// The digest `payload` hashes to as the material `reference` names.
fn digest(reference: &MaterialRef, payload: &str) -> MaterialDigest {
    mint(
        &reference.owner,
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
    /// A typed payload whose canonical JSON does not encode.
    #[error("an outcome's typed payload does not encode")]
    Unencodable,
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

    /// The payload's value, when the material the outcome names holds it
    /// typed as a `V`.
    #[must_use]
    pub fn typed<V: Any>(&self) -> Option<&V> {
        match self {
            Self::Completed(output) => output.typed(),
            Self::Failed(failure) => failure.typed(),
            Self::Waiting(source) => source.typed(),
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
    pub fn stopped_answer(&self) -> Option<lash_protocol_types::ToolCallOutput> {
        Some(match self {
            Self::Interrupted => lash_protocol_types::ToolCallOutput::failure(
                lash_protocol_types::ToolFailure::runtime(
                    lash_protocol_types::ToolFailureClass::Execution,
                    "tool_interrupted",
                    "tool was interrupted by a runtime restart; it may or may not have taken effect, and may still be running.",
                )
                .with_cause(lash_protocol_types::ToolFailureCause::Interrupted),
            ),
            Self::TimedOut { cause, .. } => lash_protocol_types::ToolCallOutput::failure(
                lash_protocol_types::ToolFailure::runtime(
                    lash_protocol_types::ToolFailureClass::Timeout,
                    "tool_timed_out",
                    format!(
                        "tool exceeded its {cause:?} limit; it may have partly run, and may still be running."
                    ),
                )
                .with_cause(lash_protocol_types::ToolFailureCause::ExecutionLimit { cause: *cause }),
            ),
            Self::Cancelled { .. } => lash_protocol_types::ToolCallOutput::cancelled(
                lash_protocol_types::ToolCancellation::runtime("the call was cancelled"),
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
    use super::{MaterialLocation, MaterialOwner, MaterialRefusal, MaterialRole};
    use crate::effect_opener::EffectOpener;
    use crate::tool_run::MaterialPayload;

    use super::{Material, SettledOutput, SettledOutputRefusal};

    /// MATERIAL-IDENTITY (FIG-7056): a material is named by the BLAKE3 of its
    /// payload's JSON object, whose `text` is a JSON string. A text material
    /// hashes the borrowed text, and a typed one streams its value's JSON
    /// through the string escaping; both mint exactly that digest for every
    /// byte JSON escapes, and a typed material projects exactly its value's
    /// JSON. A value whose JSON is not the named text is refused.
    #[test]
    fn a_typed_material_is_named_by_the_digest_of_its_json_text() {
        let owner = MaterialOwner::Run {
            opener: EffectOpener::turn("s", "t"),
        };
        let role = MaterialRole::AttemptOutput;
        let controls: String = (0_u8..0x20).map(char::from).collect();
        for value in [
            serde_json::json!(controls),
            serde_json::json!("quote \" backslash \\ slash / del \u{7f} é 😀"),
            serde_json::json!({"nested": ["a\nb", 1, -0.0, 1.5e300, null, true], "": {}}),
            serde_json::json!(""),
        ] {
            let text = serde_json::to_string(&value).unwrap();
            let payload = MaterialPayload::new(owner.clone(), role, text.clone());
            let mut preimage =
                lash_protocol_types::core_support::Blake3DomainHasher::new("lash-tool-material/v1");
            preimage.update(serde_json::to_vec(&payload).unwrap());
            let expected = preimage.finalize_hex();
            let typed = Material::journal_local_typed(owner.clone(), role, value.clone()).unwrap();
            let plain = Material::journal_local(owner.clone(), role, text.clone());
            assert_eq!(typed.reference().digest.as_str(), expected, "{text}");
            assert_eq!(plain.reference(), typed.reference());
            assert_eq!(
                &payload.reference(MaterialLocation::JournalLocal).unwrap(),
                typed.reference()
            );
            assert_eq!(typed.typed::<serde_json::Value>(), Some(&value));
            assert_eq!(plain.typed::<serde_json::Value>(), None);
            assert_eq!(typed.payload(), text);
            assert_eq!(
                Material::verified(typed.reference().clone(), value).unwrap(),
                plain
            );
        }
        let named = Material::journal_local(owner, role, "\"alpha\"".to_owned());
        assert!(matches!(
            Material::verified(named.reference().clone(), serde_json::json!("omega")),
            Err(SettledOutputRefusal::Material(
                MaterialRefusal::Corrupt { .. }
            ))
        ));
    }

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
