//! Durable string identities of the runtime's addressable things.
//!
//! Session, process, node, input, batch and turn identities are all opaque
//! host- or store-minted strings, so before they were types any one of them
//! could stand in for any other at every seam that carried them: a call taking
//! a session identity and a process identity compiled with the two swapped.
//! Each is a transparent newtype here, so a transposition is a compile error
//! while the serialized and database bytes stay exactly the string they were.
//!
//! The shape follows the turn identity that established it: `#[repr(transparent)]`
//! over `String`, `#[serde(transparent)]`, a JSON schema delegated inline to
//! `String`, and borrowing conversions (`Deref`, `AsRef`, `Borrow`) so a typed
//! id still reads as text at store and formatting boundaries without cloning.

/// Every identity gets the same surface deliberately: differing accessor sets
/// were how the borrowed/cloned drift these types replaced arose in the first
/// place. The generated `serde` and schema impls delegate to `String`, and each
/// identity pins that byte-for-byte in its own test below.
macro_rules! string_identity {
    ($(#[$meta:meta])* $name:ident, $what:literal) => {
        $(#[$meta])*
        #[repr(transparent)]
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_string())
            }
        }

        impl From<&String> for $name {
            fn from(value: &String) -> Self {
                Self(value.clone())
            }
        }

        impl std::str::FromStr for $name {
            type Err = std::convert::Infallible;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Ok(Self::from(value))
            }
        }

        string_identity_surface!($name);
    };
}

/// The read-side surface every identity shares: accessors, borrowing
/// conversions, comparisons and the inline string schema. Construction is the
/// caller's: [`string_identity!`] adds free construction from any string, and
/// [`ProcessId`] adds only its validating parse and the registrar's mint.
macro_rules! string_identity_surface {
    ($name:ident) => {
        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl std::ops::Deref for $name {
            type Target = str;

            fn deref(&self) -> &Self::Target {
                self.as_str()
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                self.as_str()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl From<&$name> for $name {
            fn from(value: &$name) -> Self {
                value.clone()
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.into_inner()
            }
        }

        impl From<&$name> for String {
            fn from(value: &$name) -> Self {
                value.as_str().to_string()
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.as_str() == other
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.as_str() == *other
            }
        }

        impl PartialEq<String> for $name {
            fn eq(&self, other: &String) -> bool {
                self.as_str() == other.as_str()
            }
        }

        impl PartialEq<$name> for str {
            fn eq(&self, other: &$name) -> bool {
                self == other.as_str()
            }
        }

        impl PartialEq<$name> for &str {
            fn eq(&self, other: &$name) -> bool {
                *self == other.as_str()
            }
        }

        impl PartialEq<$name> for String {
            fn eq(&self, other: &$name) -> bool {
                self.as_str() == other.as_str()
            }
        }

        // `String` compares against `&str` without an explicit dereference, so
        // code that compared an identity against a borrowed one kept compiling
        // through `Deref`. Spelling the borrowed pair out keeps that working
        // now that both sides are the newtype, rather than forcing call sites
        // to sprinkle dereferences at every comparison.
        impl PartialEq<&$name> for $name {
            fn eq(&self, other: &&$name) -> bool {
                self.as_str() == other.as_str()
            }
        }

        impl PartialEq<$name> for &$name {
            fn eq(&self, other: &$name) -> bool {
                self.as_str() == other.as_str()
            }
        }

        impl schemars::JsonSchema for $name {
            fn is_referenceable() -> bool {
                false
            }

            fn schema_name() -> String {
                <String as schemars::JsonSchema>::schema_name()
            }

            fn json_schema(
                generator: &mut schemars::r#gen::SchemaGenerator,
            ) -> schemars::schema::Schema {
                <String as schemars::JsonSchema>::json_schema(generator)
            }
        }
    };
}

string_identity!(
    /// Host-supplied identity of one session.
    SessionId,
    "session"
);

/// Derives the stable owner namespace used by session-owned durable records.
pub fn session_owner_namespace(session_id: impl AsRef<str>) -> String {
    format!("session:{}", session_id.as_ref())
}

/// The identity of one process: opaque, minted by the process registrar when
/// the process is first registered, and never reused (ADR 0107).
///
/// It is the only identity a process has. A host that wants idempotent starts
/// supplies a separate start key, and a host that wants a readable name
/// supplies a label; neither is an identity, so nothing resolves a name to a
/// process and a pruned process's id can never address a successor.
///
/// There is deliberately no construction from an arbitrary string: an id is
/// either minted ([`ProcessId::from_minted`], called only by the registrar) or
/// parsed back from bytes a registrar minted ([`ProcessId::parse`], which is
/// also what deserialization runs), and both enforce the one spelling,
/// `p_` followed by 32 lowercase hex digits of a UUIDv7.
#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct ProcessId(String);

/// The prefix every minted process id carries.
pub const PROCESS_ID_PREFIX: &str = "p_";
const PROCESS_ID_HEX_LEN: usize = 32;

/// A string that is not a minted process id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidProcessId {
    value: String,
}

impl std::fmt::Display for InvalidProcessId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "`{}` is not a process id: a process id is `{PROCESS_ID_PREFIX}` followed by {PROCESS_ID_HEX_LEN} lowercase hex digits, minted by the process registrar",
            self.value.escape_debug()
        )
    }
}

impl std::error::Error for InvalidProcessId {}

impl ProcessId {
    /// The id for one freshly minted UUIDv7. Only the process registrar calls
    /// this, inside the transaction that registers the process.
    pub fn from_minted(uuid_v7: u128) -> Self {
        Self(format!("{PROCESS_ID_PREFIX}{uuid_v7:032x}"))
    }

    /// A well-formed id for a test fixture, deterministic in `label`.
    ///
    /// Fixtures only: it names no registered process, and registration never
    /// takes one — the registrar mints every registered id. FNV-1a over 128
    /// bits with the version nibble pinned, so it reads as a UUIDv7 and needs
    /// no registered hash domain.
    #[doc(hidden)]
    pub fn fixture(label: &str) -> Self {
        let mut value: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
        for byte in label.as_bytes() {
            value ^= u128::from(*byte);
            value = value.wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013b);
        }
        value = (value & !(0xf_u128 << 76)) | (0x7_u128 << 76);
        value = (value & !(0b11_u128 << 62)) | (0b10_u128 << 62);
        Self::from_minted(value)
    }

    /// Parse a process id a registrar minted.
    ///
    /// # Errors
    ///
    /// [`InvalidProcessId`] for any other spelling, including every
    /// host-chosen process name the pre-minting registry accepted.
    pub fn parse(value: &str) -> Result<Self, InvalidProcessId> {
        // A UUIDv7: version 7 in the version nibble (hex digit 12) and the
        // RFC 9562 variant in the top two bits of hex digit 16.
        let valid = value.strip_prefix(PROCESS_ID_PREFIX).is_some_and(|hex| {
            let bytes = hex.as_bytes();
            hex.len() == PROCESS_ID_HEX_LEN
                && bytes
                    .iter()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
                && bytes[12] == b'7'
                && matches!(bytes[16], b'8' | b'9' | b'a' | b'b')
        });
        if valid {
            Ok(Self(value.to_string()))
        } else {
            Err(InvalidProcessId {
                value: value.to_string(),
            })
        }
    }
}

impl<'de> serde::Deserialize<'de> for ProcessId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

impl std::str::FromStr for ProcessId {
    type Err = InvalidProcessId;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<&str> for ProcessId {
    type Error = InvalidProcessId;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<String> for ProcessId {
    type Error = InvalidProcessId;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

string_identity_surface!(ProcessId);

string_identity!(NodeId, "session-graph node");

string_identity!(
    /// Identity of one queued turn input.
    InputId,
    "turn input"
);

string_identity!(
    /// Identity of one queued-work batch.
    BatchId,
    "queued-work batch"
);

string_identity!(
    /// Stable data-layer identity of one logical turn at lease, claim, and
    /// turn-registry boundaries.
    TurnId,
    "turn"
);

#[cfg(test)]
mod tests {
    use super::*;

    /// Each identity is spelled out by hand rather than looped over the macro:
    /// a round trip through the generated impls would agree with itself even if
    /// the whole macro started emitting a wrapper object.
    #[test]
    fn serde_is_the_original_string_bytes() {
        assert_eq!(
            serde_json::to_string(&SessionId::from("session-7")).unwrap(),
            r#""session-7""#
        );
        let process = ProcessId::from_minted(0x0000_0000_0000_7000_8000_0000_0000_0000 | 7);
        assert_eq!(
            serde_json::to_string(&process).unwrap(),
            r#""p_00000000000070008000000000000007""#
        );
        assert_eq!(
            serde_json::to_string(&NodeId::from("node-7")).unwrap(),
            r#""node-7""#
        );
        assert_eq!(
            serde_json::to_string(&InputId::from("input-7")).unwrap(),
            r#""input-7""#
        );
        assert_eq!(
            serde_json::to_string(&BatchId::from("batch-7")).unwrap(),
            r#""batch-7""#
        );
        assert_eq!(
            serde_json::to_string(&TurnId::from("turn-7")).unwrap(),
            r#""turn-7""#
        );

        assert_eq!(
            serde_json::from_str::<SessionId>(r#""session-7""#).unwrap(),
            SessionId::from("session-7")
        );
        assert_eq!(
            serde_json::from_str::<ProcessId>(r#""p_00000000000070008000000000000007""#).unwrap(),
            process
        );
        assert_eq!(
            serde_json::from_str::<NodeId>(r#""node-7""#).unwrap(),
            NodeId::from("node-7")
        );
        assert_eq!(
            serde_json::from_str::<InputId>(r#""input-7""#).unwrap(),
            InputId::from("input-7")
        );
        assert_eq!(
            serde_json::from_str::<BatchId>(r#""batch-7""#).unwrap(),
            BatchId::from("batch-7")
        );
        assert_eq!(
            serde_json::from_str::<TurnId>(r#""turn-7""#).unwrap(),
            TurnId::from("turn-7")
        );
    }

    /// A process id is only ever a minted spelling: a host-chosen name, an
    /// uppercase or short digest, and the pre-minting `process:…` derivations
    /// are all refused, by the parser and by deserialization alike.
    #[test]
    fn a_process_id_is_only_a_minted_spelling() {
        let minted = ProcessId::from_minted(0x0192_0000_0000_7000_8000_0000_0000_0001);
        assert_eq!(minted.as_str(), "p_01920000000070008000000000000001");
        assert_eq!(ProcessId::parse(minted.as_str()).unwrap(), minted);
        for refused in [
            "process-7",
            "p-7",
            "process:subagent:call-1",
            "p_0192000000007000800000000000001",
            "p_019200000000700080000000000000011",
            "p_0192000000007000800000000000000G",
            "P_01920000000070008000000000000001",
            "p_01920000000070008000000000000001 ",
            // Not a UUIDv7: version 4 in the version nibble, and the NCS and
            // Microsoft variants in the variant bits.
            "p_01920000000040008000000000000001",
            "p_01920000000070000000000000000001",
            "p_0192000000007000c000000000000001",
            "",
        ] {
            assert!(ProcessId::parse(refused).is_err(), "{refused:?}");
            assert!(
                serde_json::from_value::<ProcessId>(serde_json::json!(refused)).is_err(),
                "{refused:?}"
            );
        }
    }

    #[test]
    fn session_owner_namespace_has_one_canonical_spelling() {
        assert_eq!(
            session_owner_namespace(SessionId::from("session-blue")),
            "session:session-blue"
        );
    }

    /// The schema a typed identity contributes has to be the plain string
    /// schema, inline: a `$ref` to a generated definition would change every
    /// published tool and process schema that carries an identity.
    #[test]
    fn json_schema_is_the_plain_string_schema() {
        let mut generator = schemars::r#gen::SchemaGenerator::default();
        let string_schema = serde_json::to_value(<String as schemars::JsonSchema>::json_schema(
            &mut generator,
        ))
        .unwrap();
        for identity_schema in [
            <SessionId as schemars::JsonSchema>::json_schema(&mut generator),
            <ProcessId as schemars::JsonSchema>::json_schema(&mut generator),
            <NodeId as schemars::JsonSchema>::json_schema(&mut generator),
            <InputId as schemars::JsonSchema>::json_schema(&mut generator),
            <BatchId as schemars::JsonSchema>::json_schema(&mut generator),
            <TurnId as schemars::JsonSchema>::json_schema(&mut generator),
        ] {
            assert_eq!(
                serde_json::to_value(identity_schema).unwrap(),
                string_schema
            );
        }
        assert!(generator.definitions().is_empty());
    }
}
