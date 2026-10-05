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
//! over `String`, a transparent encoding, a JSON schema inline as a string, and
//! borrowing conversions (`Deref`, `AsRef`, `Borrow`) so a typed id still reads
//! as text at store and formatting boundaries without cloning.
//!
//! No identity is empty or whitespace-only: absence is `Option::None`, never a
//! blank id. Text that arrives from outside the program (a decoded wire or
//! stored value, a host-supplied string) goes through the fallible `parse`,
//! which is also what deserialization runs. Text the program itself states is
//! built from an existing id or a literal prefix cannot be blank. Session and
//! turn identities always require validated parsing in production; literal
//! fixture construction is available only with `testing`.

/// Text that is not an identity because it is empty or whitespace-only.
///
/// Absence of an identity is `Option::None`; a blank string never stands in
/// for it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a {what} id must not be empty or whitespace-only; absence is `None`, never a blank id")]
pub struct BlankIdentity {
    what: &'static str,
}

impl BlankIdentity {
    /// The kind of identity that was refused, as its documentation names it.
    pub fn what(&self) -> &'static str {
        self.what
    }
}

fn is_blank(value: &str) -> bool {
    value.trim().is_empty()
}

/// Every identity gets the same surface deliberately: differing accessor sets
/// were how the borrowed/cloned drift these types replaced arose in the first
/// place. The encoding is the bare string, and each identity pins that
/// byte-for-byte in its own test below.
macro_rules! string_identity {
    ($(#[$meta:meta])* $name:ident, $what:literal $(, $literal_cfg:meta)?) => {
        $(#[$meta])*
        #[repr(transparent)]
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Validates text that arrived from outside the program.
            ///
            /// # Errors
            ///
            /// [`BlankIdentity`] when the text is empty or whitespace-only.
            pub fn parse(value: impl Into<String>) -> Result<Self, BlankIdentity> {
                let value = value.into();
                if is_blank(&value) {
                    return Err(BlankIdentity { what: $what });
                }
                Ok(Self(value))
            }

            /// The id spelled `prefix` followed by `rest`.
            ///
            /// # Panics
            ///
            /// When the literal `prefix` is blank, which is a defect in the
            /// calling code rather than in any input.
            pub fn prefixed(prefix: &'static str, rest: impl std::fmt::Display) -> Self {
                assert!(
                    !is_blank(prefix),
                    concat!("the literal prefix of a ", $what, " id must not be blank")
                );
                Self(format!("{prefix}{rest}"))
            }

            /// The id spelled as the hyphenated lowercase text of the UUID
            /// whose 128 bits are `bits`.
            pub fn from_uuid(bits: u128) -> Self {
                Self(format!(
                    "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
                    (bits >> 96) as u32,
                    (bits >> 80) as u16,
                    (bits >> 64) as u16,
                    (bits >> 48) as u16,
                    bits & 0xffff_ffff_ffff
                ))
            }

            /// The id spelled as this one followed by `suffix`.
            #[must_use]
            pub fn with_suffix(&self, suffix: impl std::fmt::Display) -> Self {
                Self(format!("{}{suffix}", self.0))
            }

            /// An id for a test fixture.
            ///
            /// # Panics
            ///
            /// When `label` is blank.
            $(#[$literal_cfg])*
            pub fn fixture(label: impl Into<String>) -> Self {
                match Self::parse(label) {
                    Ok(id) => id,
                    Err(error) => panic!("{error}"),
                }
            }
        }

        /// A literal id. Text that is not a literal goes through
        /// [`parse`](Self::parse).
        ///
        /// # Panics
        ///
        /// When the literal is blank, which is a defect in the calling code
        /// rather than in any input.
        $(#[$literal_cfg])*
        impl From<&'static str> for $name {
            fn from(value: &'static str) -> Self {
                assert!(
                    !is_blank(value),
                    concat!("a literal ", $what, " id must not be blank")
                );
                Self(value.to_string())
            }
        }

        impl TryFrom<String> for $name {
            type Error = BlankIdentity;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::parse(value)
            }
        }

        impl std::str::FromStr for $name {
            type Err = BlankIdentity;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(value).map_err(serde::de::Error::custom)
            }
        }

        impl schemars::JsonSchema for $name {
            fn inline_schema() -> bool {
                true
            }

            fn schema_name() -> std::borrow::Cow<'static, str> {
                std::borrow::Cow::Borrowed(stringify!($name))
            }

            /// A string of at least one character. That it is also not
            /// whitespace-only is `parse`'s refusal; a schema length cannot
            /// state it.
            fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({ "type": "string", "minLength": 1 })
            }
        }

        string_identity_surface!($name);
    };
}

/// The read-side surface every identity shares: accessors, borrowing
/// conversions and comparisons. Construction, decoding and the schema are the
/// caller's: [`string_identity!`] refuses blank text, and [`ProcessId`] admits
/// only its registrar's one spelling.
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
    };
}

string_identity!(
    /// Host-supplied identity of one session.
    SessionId,
    "session",
    cfg(any(test, feature = "testing"))
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
/// either minted (only by the registrar, under its [`ProcessIdRegistrar`]
/// authority) or parsed back from bytes a registrar minted
/// ([`ProcessId::parse`], which is also what deserialization runs), and both
/// enforce the one spelling, `p_` followed by 32 lowercase hex digits of a
/// UUIDv7.
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

/// The process registrar's authority to mint a process id.
///
/// Only the registrar's mint (`lash_core_store::ProcessIdMint`) holds it: no
/// facade re-exports it, so host, tool and plugin code cannot mint an id,
/// only parse one a registrar minted. A test names a process it never
/// registered with [`ProcessId::fixture`].
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct ProcessIdRegistrar(());

impl ProcessIdRegistrar {
    /// The one authority the registrar's mint holds.
    #[doc(hidden)]
    pub const REGISTRAR: Self = Self(());
}

impl ProcessId {
    /// The id for one freshly minted UUIDv7, minted by the process registrar
    /// inside the transaction that registers the process.
    #[doc(hidden)]
    ///
    /// # Errors
    ///
    /// [`InvalidProcessId`] if the bits do not encode UUID version 7 and the
    /// RFC 9562 variant.
    pub fn minted(_registrar: ProcessIdRegistrar, uuid_v7: u128) -> Result<Self, InvalidProcessId> {
        Self::parse(&format!("{PROCESS_ID_PREFIX}{uuid_v7:032x}"))
    }

    pub(crate) fn from_minted(uuid_v7: u128) -> Self {
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

impl schemars::JsonSchema for ProcessId {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        <String as schemars::JsonSchema>::schema_name()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        <String as schemars::JsonSchema>::json_schema(generator)
    }
}

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
    "turn",
    cfg(any(test, feature = "testing"))
);

/// The root of a shift that starts with an input the host gave no id of its
/// own is named by the input's id.
impl From<&InputId> for TurnId {
    fn from(input_id: &InputId) -> Self {
        Self(input_id.as_str().to_string())
    }
}

/// The input that opened a root is addressed by the root's id.
impl From<&TurnId> for InputId {
    fn from(run: &TurnId) -> Self {
        Self(run.as_str().to_string())
    }
}

/// The turn a process runs for itself is named by the process id.
impl From<&ProcessId> for TurnId {
    fn from(process_id: &ProcessId) -> Self {
        Self(process_id.as_str().to_string())
    }
}

/// Who a runtime runs for: a session, or a process named by its minted id.
///
/// A session runtime and a process runtime cross the same interfaces — the
/// plugin session, the tool catalog, the attachment facade, the authority a
/// tool intent is declared under — and those interfaces take a
/// `RuntimeOwner`. A process runtime is never handed a session id as a
/// stand-in for its own identity.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeOwner {
    Session(SessionId),
    Process(ProcessId),
}

impl RuntimeOwner {
    pub fn session_id(&self) -> Option<&SessionId> {
        match self {
            Self::Session(session_id) => Some(session_id),
            Self::Process(_) => None,
        }
    }

    pub fn process_id(&self) -> Option<&ProcessId> {
        match self {
            Self::Session(_) => None,
            Self::Process(process_id) => Some(process_id),
        }
    }
}

/// `session:<session id>` or `process:<process id>`. A lashlang VM's owner
/// stamp for a process is the same `process:<process id>` text, so the two
/// must stay equal.
impl std::fmt::Display for RuntimeOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Session(session_id) => write!(formatter, "session:{session_id}"),
            Self::Process(process_id) => write!(formatter, "process:{process_id}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_public_process_id_mint_round_trips_through_parse() {
        let registrar = ProcessIdRegistrar::REGISTRAR;
        for payload in [0, 1, u128::MAX, 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210] {
            for version in 0..16_u128 {
                for variant in 0..4_u128 {
                    let bits = (payload & !(0xf_u128 << 76) & !(0b11_u128 << 62))
                        | (version << 76)
                        | (variant << 62);
                    let minted = ProcessId::minted(registrar, bits);
                    if version == 7 && variant == 2 {
                        let id = minted.expect("UUIDv7 with the RFC variant mints");
                        assert_eq!(
                            ProcessId::parse(id.as_str()).expect("public mint parses"),
                            id
                        );
                        assert_eq!(
                            serde_json::from_str::<ProcessId>(&serde_json::to_string(&id).unwrap())
                                .unwrap(),
                            id
                        );
                    } else {
                        assert!(minted.is_err(), "invalid bits must refuse: {bits:032x}");
                    }
                }
            }
        }
        for label in ["", "process", "p_0", "東京", "\0"] {
            let id = ProcessId::fixture(label);
            assert_eq!(
                ProcessId::parse(id.as_str()).expect("fixture mint parses"),
                id
            );
        }
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

    /// A blank string is never an identity: every decoder refuses it, so the
    /// only spelling of an absent id is `None`.
    #[test]
    fn a_blank_string_decodes_to_no_identity() {
        for blank in ["", " ", "\t\n", "\u{a0}\u{2003}"] {
            let encoded = serde_json::json!(blank);
            assert!(serde_json::from_value::<SessionId>(encoded.clone()).is_err());
            assert!(serde_json::from_value::<TurnId>(encoded.clone()).is_err());
            assert!(serde_json::from_value::<NodeId>(encoded.clone()).is_err());
            assert!(serde_json::from_value::<InputId>(encoded.clone()).is_err());
            assert!(serde_json::from_value::<BatchId>(encoded.clone()).is_err());
            assert!(
                serde_json::from_value::<Option<SessionId>>(encoded).is_err(),
                "a blank id is not a second spelling of `None`"
            );
        }
        assert_eq!(
            serde_json::from_value::<Option<SessionId>>(serde_json::Value::Null).unwrap(),
            None
        );
    }

    #[test]
    fn untrusted_text_becomes_an_identity_only_through_the_typed_parse() {
        for blank in ["", " ", "\t\n"] {
            assert_eq!(SessionId::parse(blank).unwrap_err().what(), "session");
            assert_eq!(TurnId::parse(blank).unwrap_err().what(), "turn");
            assert_eq!(
                NodeId::parse(blank).unwrap_err().what(),
                "session-graph node"
            );
            assert_eq!(InputId::parse(blank).unwrap_err().what(), "turn input");
            assert_eq!(
                BatchId::parse(blank).unwrap_err().what(),
                "queued-work batch"
            );
            assert!(blank.parse::<SessionId>().is_err());
            assert!(TurnId::try_from(blank.to_string()).is_err());
        }
        // Only blankness is refused: the bytes of any other id are kept as given.
        for kept in ["s-1", " padded ", "session\0x", "λ"] {
            let id = SessionId::parse(kept).expect("a non-blank id parses");
            assert_eq!(id.as_str(), kept);
            assert_eq!(serde_json::to_value(&id).unwrap(), serde_json::json!(kept));
            assert_eq!(
                serde_json::from_value::<SessionId>(serde_json::json!(kept)).unwrap(),
                id
            );
        }
    }
}
