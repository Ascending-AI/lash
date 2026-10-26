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
//! over immutable shared text, a transparent encoding, a JSON schema inline as a string, and
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
    #[doc(hidden)]
    pub const fn new(what: &'static str) -> Self {
        Self { what }
    }
    /// The kind of identity that was refused, as its documentation names it.
    pub fn what(&self) -> &'static str {
        self.what
    }
}

/// Every identity gets the same surface deliberately: differing accessor sets
/// were how the borrowed/cloned drift these types replaced arose in the first
/// place. The encoding is the bare string, and each identity pins that
/// byte-for-byte in its own test below.
#[doc(hidden)]
#[macro_export]
macro_rules! string_identity {
    ($(#[$meta:meta])* $name:ident, $what:literal $(, $literal_cfg:meta)?) => {
        $(#[$meta])*
        #[repr(transparent)]
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
        #[serde(transparent)]
        pub struct $name(std::sync::Arc<str>);

        impl $name {
            /// Validates text that arrived from outside the program.
            ///
            /// # Errors
            ///
            /// [`BlankIdentity`] when the text is empty or whitespace-only.
            pub fn parse(value: impl Into<String>) -> Result<Self, $crate::identity::BlankIdentity> { Self::from_text(&value.into()) }
            fn from_text(value: &str) -> Result<Self, $crate::identity::BlankIdentity> {
                if value.trim().is_empty() { return Err($crate::identity::BlankIdentity::new($what)); }
                Ok(Self(std::sync::Arc::from(value)))
            }

            /// The id spelled `prefix` followed by `rest`.
            ///
            /// # Panics
            ///
            /// When the literal `prefix` is blank, which is a defect in the
            /// calling code rather than in any input.
            pub fn prefixed(prefix: &'static str, rest: impl std::fmt::Display) -> Self {
                assert!(
                    !prefix.trim().is_empty(),
                    concat!("the literal prefix of a ", $what, " id must not be blank")
                );
                Self(format!("{prefix}{rest}").into())
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
                ).into())
            }

            /// The id spelled as this one followed by `suffix`.
            #[must_use]
            pub fn with_suffix(&self, suffix: impl std::fmt::Display) -> Self {
                Self(format!("{}{suffix}", self.0).into())
            }

            /// An id for a test fixture.
            ///
            /// # Panics
            ///
            /// When `label` is blank.
            #[cfg(any(test, feature = "testing"))]
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
                    !value.trim().is_empty(),
                    concat!("a literal ", $what, " id must not be blank")
                );
                Self(std::sync::Arc::from(value))
            }
        }

        impl TryFrom<String> for $name {
            type Error = $crate::identity::BlankIdentity;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::parse(value)
            }
        }

        impl std::str::FromStr for $name {
            type Err = $crate::identity::BlankIdentity;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                $crate::names::deserialize_text(deserializer, Self::from_text)
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

        $crate::string_identity_surface!($name);
    };
}

/// The read-side surface every identity shares: accessors, borrowing
/// conversions and comparisons. Construction, decoding and the schema are the
/// caller's: [`string_identity!`] refuses blank text, and `ProcessId` admits
/// only its registrar's one spelling.
#[doc(hidden)]
#[macro_export]
macro_rules! string_identity_surface {
    ($name:ident) => {
        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_inner(self) -> String {
                self.0.to_string()
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
