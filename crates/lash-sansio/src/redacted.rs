//! A typed wrapper for secrets whose formatting never reveals the value.
//!
//! Provider API keys, OAuth tokens, and object-store credentials ride inside
//! structs that derive [`Debug`] for diagnostics. Wrapping the secret in
//! [`Redacted`] makes every such derive safe by construction: the only way to
//! reach the plaintext is the explicit [`Redacted::expose_secret`] call at the
//! wire-write site (FIG-878).

use std::fmt;

/// The placeholder every formatting path prints instead of the secret.
const PLACEHOLDER: &str = "[redacted]";

/// A secret string whose `Debug` and `Display` output never contain the
/// wrapped value.
///
/// Comparisons and hashing still operate on the plaintext so wrapped
/// credentials keep working as struct members of `PartialEq`/`Hash` types.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Redacted(String);

impl Redacted {
    /// Wraps a secret.
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// Returns the plaintext secret.
    ///
    /// Call this only at the point the secret leaves the process on purpose
    /// (an authorization header, a signed request); never in a log or error
    /// message.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    /// Consumes the wrapper and returns the plaintext secret.
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Debug for Redacted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(PLACEHOLDER)
    }
}

impl fmt::Display for Redacted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(PLACEHOLDER)
    }
}

impl From<String> for Redacted {
    fn from(secret: String) -> Self {
        Self(secret)
    }
}

impl From<&str> for Redacted {
    fn from(secret: &str) -> Self {
        Self(secret.to_string())
    }
}

/// Deserialization is safe: it reads a secret in. There is deliberately no
/// `Serialize` impl — writing a secret out must go through
/// [`Redacted::expose_secret`] so every leak point is an explicit call.
impl<'de> serde::Deserialize<'de> for Redacted {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "sk-live-supersecret-000";

    #[test]
    fn debug_and_display_never_reveal_the_secret() {
        let redacted = Redacted::new(SECRET);
        let debug = format!("{redacted:?}");
        let display = format!("{redacted}");
        assert!(!debug.contains(SECRET), "debug leaked: {debug}");
        assert!(!display.contains(SECRET), "display leaked: {display}");
        assert_eq!("[redacted]", debug);
        assert_eq!("[redacted]", display);
    }

    #[test]
    fn debug_of_a_containing_derive_never_reveals_the_secret() {
        #[derive(Debug)]
        #[expect(dead_code, reason = "the derive reads the fields")]
        struct Holder {
            api_key: Redacted,
        }
        let holder = Holder {
            api_key: Redacted::new(SECRET),
        };
        let debug = format!("{holder:?}");
        assert!(!debug.contains(SECRET), "derive leaked: {debug}");
        assert!(debug.contains("[redacted]"));
    }

    #[test]
    fn expose_secret_returns_the_plaintext() {
        let redacted = Redacted::new(SECRET);
        assert_eq!(SECRET, redacted.expose_secret());
        assert_eq!(SECRET, redacted.clone().into_inner());
    }

    #[test]
    fn deserialization_reads_the_plaintext_in() {
        let redacted: Redacted = serde_json::from_str("\"sk-live-supersecret-000\"").unwrap();
        assert_eq!(SECRET, redacted.expose_secret());
    }

    #[test]
    fn equality_and_conversions_operate_on_the_plaintext() {
        assert_eq!(Redacted::from(SECRET), Redacted::from(SECRET.to_string()));
        assert_ne!(Redacted::from(SECRET), Redacted::from("other"));
    }
}
