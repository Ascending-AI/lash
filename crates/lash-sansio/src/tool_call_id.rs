//! The one identity lash gives every admitted logical tool call (ADR 0117).
//!
//! A [`ToolCallId`] is derived, never chosen: a BLAKE3 digest over the
//! deployment namespace, the durable admission root the call was admitted
//! under, and the tagged positions that locate the call inside that root. No
//! provider call id, argument, tool name, attempt number or scheduling order
//! enters it, so a crash replay and a reported-failure retry present the same
//! id, and two distinct calls never share one.

use std::fmt;

use crate::ProcessId;
use crate::core_support::Blake3DomainHasher;

/// The prefix every tool call id carries.
pub const TOOL_CALL_ID_PREFIX: &str = "tc_";
const DIGEST_HEX_LEN: usize = 64;

/// Permanent form registry: 1 admitted derivation, 2 batch-member extension.
const FORM_ADMITTED: u8 = 1;
const FORM_BATCH_MEMBER: u8 = 2;

/// Permanent root tag registry: 1 turn, 2 host submission, 3 process.
const ROOT_TURN: u8 = 1;
const ROOT_HOST_SUBMISSION: u8 = 2;
const ROOT_PROCESS: u8 = 3;

/// Permanent position tag registry. Retired tags stay burned.
const POSITION_CONTINUATION: u8 = 1;
const POSITION_ITERATION: u8 = 2;
const POSITION_EFFECT_ORDINAL: u8 = 3;
const POSITION_CONTENT_INDEX: u8 = 4;
/// Written only by [`ToolCallId::child`]; no [`ToolCallPosition`] spells it.
const POSITION_BATCH_MEMBER: u8 = 5;
const POSITION_CODE_OPENER: u8 = 6;
const POSITION_CODE_CELL: u8 = 7;
const POSITION_CODE_COMMAND: u8 = 8;
const POSITION_CODE_AGGREGATE: u8 = 9;

/// The durable admission a tool call's identity is rooted in.
///
/// Each root is admitted and persisted before any effect of a call under it.
/// A redelivery of the same admission presents the same root; a new
/// submission is admitted under a new one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolCallRoot<'a>(RootKind<'a>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RootKind<'a> {
    Turn(&'a str),
    HostSubmission(&'a str),
    Process(&'a ProcessId),
}

/// An admission handle that cannot root tool call identities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolCallRootError {
    /// A blank handle would collapse every call admitted without one onto a
    /// single root.
    BlankHandle,
}

impl fmt::Display for ToolCallRootError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BlankHandle => {
                formatter.write_str("a tool call root handle must not be empty or whitespace-only")
            }
        }
    }
}

impl std::error::Error for ToolCallRootError {}

impl<'a> ToolCallRoot<'a> {
    /// The admitted operation handle of one turn.
    pub fn turn(handle: &'a str) -> Result<Self, ToolCallRootError> {
        Self::handle(handle).map(|handle| Self(RootKind::Turn(handle)))
    }

    /// The admitted operation handle of one host tool submission.
    pub fn host_submission(handle: &'a str) -> Result<Self, ToolCallRootError> {
        Self::handle(handle).map(|handle| Self(RootKind::HostSubmission(handle)))
    }

    /// The minted id of one process: its tool-call input, its code, and every
    /// trigger delivery bound to it.
    pub fn process(process_id: &'a ProcessId) -> Self {
        Self(RootKind::Process(process_id))
    }

    fn handle(handle: &'a str) -> Result<&'a str, ToolCallRootError> {
        if handle.trim().is_empty() {
            Err(ToolCallRootError::BlankHandle)
        } else {
            Ok(handle)
        }
    }

    fn encode(&self, digest: &mut Blake3DomainHasher) {
        let (tag, handle) = match self.0 {
            RootKind::Turn(handle) => (ROOT_TURN, handle),
            RootKind::HostSubmission(handle) => (ROOT_HOST_SUBMISSION, handle),
            RootKind::Process(process_id) => (ROOT_PROCESS, process_id.as_str()),
        };
        digest.update([tag]);
        encode_bytes(digest, handle);
    }
}

/// One tagged position locating a call inside its admission root.
///
/// The tag is part of the encoding, so the same number under two tags names
/// two positions. A batch member is not listed here: [`ToolCallId::child`] is
/// its only spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolCallPosition<'a> {
    /// The physical continuation of the turn the call was issued in.
    Continuation(u64),
    /// The protocol iteration within that continuation.
    Iteration(u64),
    /// The durable effect ordinal of the model response within the iteration.
    EffectOrdinal(u64),
    /// The call's full original content index in the response, before any
    /// filtering.
    ContentIndex(u64),
    /// The admitted opener running the code, in its canonical identity
    /// encoding.
    CodeOpener(&'a str),
    /// The cell's own replay key inside its turn.
    CodeCell(&'a str),
    /// The whole-program issue ordinal of the command. Process segment
    /// boundaries never reset it.
    CodeCommand(u64),
    /// A leaf's first-appearance index in the aggregate as written.
    CodeAggregate(u64),
}

impl ToolCallPosition<'_> {
    fn encode(&self, digest: &mut Blake3DomainHasher) {
        match *self {
            Self::Continuation(value) => encode_number(digest, POSITION_CONTINUATION, value),
            Self::Iteration(value) => encode_number(digest, POSITION_ITERATION, value),
            Self::EffectOrdinal(value) => encode_number(digest, POSITION_EFFECT_ORDINAL, value),
            Self::ContentIndex(value) => encode_number(digest, POSITION_CONTENT_INDEX, value),
            Self::CodeOpener(value) => {
                digest.update([POSITION_CODE_OPENER]);
                encode_bytes(digest, value);
            }
            Self::CodeCell(value) => {
                digest.update([POSITION_CODE_CELL]);
                encode_bytes(digest, value);
            }
            Self::CodeCommand(value) => encode_number(digest, POSITION_CODE_COMMAND, value),
            Self::CodeAggregate(value) => encode_number(digest, POSITION_CODE_AGGREGATE, value),
        }
    }
}

fn encode_bytes(digest: &mut Blake3DomainHasher, value: &str) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value.as_bytes());
}

fn encode_number(digest: &mut Blake3DomainHasher, tag: u8, value: u64) {
    digest.update([tag]);
    digest.update(value.to_be_bytes());
}

/// The identity of one admitted logical tool call (ADR 0117).
///
/// Sealed: there is no construction from an arbitrary string and no
/// `Default`. An id is derived by [`ToolCallId::derive`] or
/// [`ToolCallId::child`], or parsed back from the spelling one of them
/// produced, `tc_` followed by 64 lowercase hex digits.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ToolCallId(String);

/// A string that is not a tool call id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidToolCallId {
    value: String,
}

impl fmt::Display for InvalidToolCallId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "`{}` is not a tool call id: a tool call id is `{TOOL_CALL_ID_PREFIX}` followed by {DIGEST_HEX_LEN} lowercase hex digits, derived by lash",
            self.value.escape_debug()
        )
    }
}

impl std::error::Error for InvalidToolCallId {}

impl ToolCallId {
    /// Derives the id of the call at `positions` inside `root`, in the
    /// deployment `namespace` (ADR 0111; empty for the default namespace).
    pub fn derive(
        namespace: &str,
        root: ToolCallRoot<'_>,
        positions: &[ToolCallPosition<'_>],
    ) -> Self {
        let mut digest = Blake3DomainHasher::new("lash-tool-call-id/v1");
        digest.update([FORM_ADMITTED]);
        encode_bytes(&mut digest, namespace);
        root.encode(&mut digest);
        digest.update((positions.len() as u64).to_be_bytes());
        for position in positions {
            position.encode(&mut digest);
        }
        Self::from_digest(digest)
    }

    /// The id of the batch member at its original `member_index` in the
    /// wrapper this id names, counted before any member is refused.
    pub fn child(&self, member_index: u64) -> Self {
        let mut digest = Blake3DomainHasher::new("lash-tool-call-id/v1");
        digest.update([FORM_BATCH_MEMBER]);
        // Fixed width, so the parent needs no length prefix.
        digest.update(self.digest_hex().as_bytes());
        encode_number(&mut digest, POSITION_BATCH_MEMBER, member_index);
        Self::from_digest(digest)
    }

    /// Parses a spelling lash derived.
    ///
    /// # Errors
    ///
    /// [`InvalidToolCallId`] for any other string, including every provider
    /// call id.
    pub fn parse(value: &str) -> Result<Self, InvalidToolCallId> {
        let valid = value.strip_prefix(TOOL_CALL_ID_PREFIX).is_some_and(|hex| {
            hex.len() == DIGEST_HEX_LEN
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(InvalidToolCallId {
                value: value.to_owned(),
            })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn digest_hex(&self) -> &str {
        &self.0[TOOL_CALL_ID_PREFIX.len()..]
    }

    fn from_digest(digest: Blake3DomainHasher) -> Self {
        Self(format!("{TOOL_CALL_ID_PREFIX}{}", digest.finalize_hex()))
    }
}

impl fmt::Debug for ToolCallId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("ToolCallId").field(&self.0).finish()
    }
}

impl fmt::Display for ToolCallId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl AsRef<str> for ToolCallId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for ToolCallId {
    type Err = InvalidToolCallId;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<&str> for ToolCallId {
    type Error = InvalidToolCallId;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl serde::Serialize for ToolCallId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for ToolCallId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

impl schemars::JsonSchema for ToolCallId {
    fn is_referenceable() -> bool {
        false
    }

    fn schema_name() -> String {
        String::schema_name()
    }

    fn json_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        String::json_schema(generator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionId;
    use crate::frame_key::FrameKey;

    const NAMESPACE: &str = "prod";

    fn turn(handle: &str) -> ToolCallRoot<'_> {
        ToolCallRoot::turn(handle).expect("nonblank handle")
    }

    fn model_call(root: ToolCallRoot<'_>, content_index: u64) -> ToolCallId {
        ToolCallId::derive(
            NAMESPACE,
            root,
            &[
                ToolCallPosition::Continuation(0),
                ToolCallPosition::Iteration(2),
                ToolCallPosition::EffectOrdinal(1),
                ToolCallPosition::ContentIndex(content_index),
            ],
        )
    }

    #[test]
    fn derivation_is_deterministic_and_pinned() {
        let first = model_call(turn("op-1"), 3);
        assert_eq!(first, model_call(turn("op-1"), 3));
        assert_eq!(
            first.as_str(),
            "tc_bf46f2c73ea625c52d0f35065d64bf8da63dfc0846a3f999ae9a80700f939943"
        );
        assert_ne!(first, model_call(turn("op-1"), 4));
        assert_ne!(first, model_call(turn("op-2"), 3));
        assert_ne!(
            first,
            ToolCallId::derive(
                "staging",
                turn("op-1"),
                &[ToolCallPosition::ContentIndex(3)]
            )
        );
    }

    #[test]
    fn domain_is_separate_from_every_other_lash_blake3_identity() {
        let id = ToolCallId::derive("", turn("call-1"), &[]);
        let frame = FrameKey::from_call_site(&SessionId::from(""), "", "call-1");
        assert_ne!(id.digest_hex(), &frame.as_str()["frame-key/v2/".len()..]);

        let mut same_bytes_other_domain = Blake3DomainHasher::new("lash-intent/v2");
        let mut same_bytes = Blake3DomainHasher::new("lash-tool-call-id/v1");
        for digest in [&mut same_bytes_other_domain, &mut same_bytes] {
            digest.update([FORM_ADMITTED]);
            encode_bytes(digest, "");
            turn("call-1").encode(digest);
            digest.update(0_u64.to_be_bytes());
        }
        assert_eq!(same_bytes.finalize_hex(), id.digest_hex());
        assert_ne!(same_bytes_other_domain.finalize_hex(), id.digest_hex());
    }

    #[test]
    fn every_component_is_length_delimited() {
        let split = |namespace, handle, cell| {
            ToolCallId::derive(namespace, turn(handle), &[ToolCallPosition::CodeCell(cell)])
        };
        assert_ne!(split("ab", "c", "x"), split("a", "bc", "x"));
        assert_ne!(split("n", "ab", "c"), split("n", "a", "bc"));
        assert_ne!(
            ToolCallId::derive(
                NAMESPACE,
                turn("op"),
                &[
                    ToolCallPosition::CodeOpener("ab"),
                    ToolCallPosition::CodeCell("c"),
                ],
            ),
            ToolCallId::derive(
                NAMESPACE,
                turn("op"),
                &[
                    ToolCallPosition::CodeOpener("a"),
                    ToolCallPosition::CodeCell("bc"),
                ],
            )
        );
    }

    #[test]
    fn the_same_number_under_different_tags_names_different_calls() {
        let under = |position| ToolCallId::derive(NAMESPACE, turn("op"), &[position]);
        let ids = [
            under(ToolCallPosition::Continuation(7)),
            under(ToolCallPosition::Iteration(7)),
            under(ToolCallPosition::EffectOrdinal(7)),
            under(ToolCallPosition::ContentIndex(7)),
            under(ToolCallPosition::CodeCommand(7)),
            under(ToolCallPosition::CodeAggregate(7)),
            under(ToolCallPosition::CodeOpener("7")),
            under(ToolCallPosition::CodeCell("7")),
        ];
        let distinct = ids.iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(distinct.len(), ids.len());

        let process = ProcessId::fixture("p");
        let roots = [
            ToolCallId::derive(NAMESPACE, turn(process.as_str()), &[]),
            ToolCallId::derive(
                NAMESPACE,
                ToolCallRoot::host_submission(process.as_str()).expect("nonblank"),
                &[],
            ),
            ToolCallId::derive(NAMESPACE, ToolCallRoot::process(&process), &[]),
        ];
        let distinct = roots.iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(distinct.len(), roots.len());

        assert_ne!(
            ToolCallId::derive(
                NAMESPACE,
                turn("op"),
                &[
                    ToolCallPosition::Iteration(1),
                    ToolCallPosition::ContentIndex(2)
                ],
            ),
            ToolCallId::derive(
                NAMESPACE,
                turn("op"),
                &[
                    ToolCallPosition::Iteration(2),
                    ToolCallPosition::ContentIndex(1)
                ],
            )
        );
    }

    #[test]
    fn child_extension_is_stable_and_member_specific() {
        let wrapper = model_call(turn("op-1"), 0);
        let member = wrapper.child(2);
        assert_eq!(member, model_call(turn("op-1"), 0).child(2));
        assert_eq!(
            member.as_str(),
            "tc_ac39d0f5b2fd4b79b19ea69bd6d20de11236a3db6f1b185cc91254a8d0e8f389"
        );
        assert_ne!(member, wrapper.child(3));
        assert_ne!(member, wrapper);
        assert_ne!(member, model_call(turn("op-1"), 1).child(2));
        assert_ne!(member.child(0), wrapper.child(0));

        let reparsed = ToolCallId::parse(wrapper.as_str()).expect("derived spelling");
        assert_eq!(reparsed.child(2), member);
    }

    #[test]
    fn serde_round_trips_through_the_prefixed_lowercase_hex_string() {
        let id = ToolCallId::derive(
            NAMESPACE,
            ToolCallRoot::process(&ProcessId::fixture("worker")),
            &[
                ToolCallPosition::CodeCommand(4),
                ToolCallPosition::CodeAggregate(1),
            ],
        );
        let encoded = serde_json::to_value(&id).expect("serialize");
        assert_eq!(encoded, serde_json::json!(id.to_string()));
        assert_eq!(
            serde_json::from_value::<ToolCallId>(encoded).expect("deserialize"),
            id
        );
        assert_eq!(id.as_str().parse::<ToolCallId>(), Ok(id.clone()));
        assert_eq!(ToolCallId::try_from(id.as_str()), Ok(id));
    }

    #[test]
    fn refuses_empty_and_malformed_strings() {
        let valid = ToolCallId::derive(NAMESPACE, turn("op"), &[]);
        let uppercase = valid.as_str().to_uppercase().replacen("TC_", "tc_", 1);
        let short = &valid.as_str()[..valid.as_str().len() - 1];
        let long = format!("{}0", valid.as_str());
        let unprefixed = valid.digest_hex().to_owned();
        let foreign = format!("p_{}", valid.digest_hex());
        let non_hex = format!("{TOOL_CALL_ID_PREFIX}{}", "g".repeat(DIGEST_HEX_LEN));
        for refused in [
            "",
            "tc_",
            "call_0",
            "toolu_01A",
            short,
            long.as_str(),
            uppercase.as_str(),
            unprefixed.as_str(),
            foreign.as_str(),
            non_hex.as_str(),
        ] {
            assert!(ToolCallId::parse(refused).is_err(), "{refused:?}");
            assert!(refused.parse::<ToolCallId>().is_err(), "{refused:?}");
            assert!(
                serde_json::from_value::<ToolCallId>(serde_json::json!(refused)).is_err(),
                "{refused:?}"
            );
        }
        let error = ToolCallId::parse("call_0").expect_err("provider id");
        assert!(error.to_string().contains("derived by lash"));
    }

    #[test]
    fn a_blank_admission_handle_roots_nothing() {
        for blank in ["", "  "] {
            assert_eq!(
                ToolCallRoot::turn(blank),
                Err(ToolCallRootError::BlankHandle)
            );
            assert_eq!(
                ToolCallRoot::host_submission(blank),
                Err(ToolCallRootError::BlankHandle)
            );
        }
    }

    #[test]
    fn json_schema_is_the_plain_string_schema() {
        let mut generator = schemars::r#gen::SchemaGenerator::default();
        assert_eq!(
            serde_json::to_value(<ToolCallId as schemars::JsonSchema>::json_schema(
                &mut generator
            ))
            .unwrap(),
            serde_json::to_value(<String as schemars::JsonSchema>::json_schema(
                &mut generator
            ))
            .unwrap()
        );
        assert!(generator.definitions().is_empty());
    }
}
