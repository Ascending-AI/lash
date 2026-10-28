//! One immutable advertisement shared by a boot's lease copies.

use std::sync::Arc;

/// Served claim tokens and their JSON encoding, prepared once for a node lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServedClaims(Arc<Tokens>);

#[derive(Debug, PartialEq, Eq)]
struct Tokens {
    tokens: Box<[String]>,
    json: Box<str>,
}

impl From<Vec<String>> for ServedClaims {
    fn from(tokens: Vec<String>) -> Self {
        let json = serde_json::to_string(&tokens)
            .unwrap_or_else(|error| unreachable!("claim strings serialize: {error}"));
        Self(Arc::new(Tokens {
            tokens: tokens.into_boxed_slice(),
            json: json.into_boxed_str(),
        }))
    }
}

impl Default for ServedClaims {
    fn default() -> Self {
        Vec::new().into()
    }
}

impl AsRef<[String]> for ServedClaims {
    fn as_ref(&self) -> &[String] {
        &self.0.tokens
    }
}

impl ServedClaims {
    /// The shared JSON array used by SQLite claim admission.
    #[must_use]
    pub fn as_json(&self) -> &str {
        &self.0.json
    }
}
