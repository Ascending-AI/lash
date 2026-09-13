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

/// Defines one transparent string identity newtype.
///
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
            #[doc = concat!("Wraps an already-minted ", $what, " identity.")]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            #[doc = concat!("Borrows the ", $what, " identity as text for store and durable-substrate implementors.")]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            #[doc = concat!("Returns the owned ", $what, " identity to store and durable-substrate implementors.")]
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

        impl std::str::FromStr for $name {
            type Err = std::convert::Infallible;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Ok(Self::from(value))
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

string_identity!(
    /// Host-supplied name of one reusable process.
    ///
    /// A bare process identity is only a name; a durable reference to one
    /// lifetime of it pins a store-minted incarnation alongside this value.
    ProcessId,
    "process"
);

string_identity!(
    /// Store-minted identity of one node in a session graph.
    NodeId,
    "session-graph node"
);

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
        assert_eq!(
            serde_json::to_string(&ProcessId::from("process-7")).unwrap(),
            r#""process-7""#
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
            serde_json::from_str::<ProcessId>(r#""process-7""#).unwrap(),
            ProcessId::from("process-7")
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
