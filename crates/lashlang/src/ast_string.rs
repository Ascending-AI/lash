use std::borrow::Borrow;
use std::fmt;
use std::ops::Deref;

use compact_str::CompactString;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The compact string representation used inside the Lashlang IR.
///
/// Its Serde and JSON Schema forms are both the same JSON string as
/// [`String`]. The local wrapper lets the schema derive follow the complete IR
/// while retaining compact storage.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[repr(transparent)]
#[serde(transparent)]
pub struct AstString(CompactString);

impl AstString {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl JsonSchema for AstString {
    fn schema_name() -> String {
        String::schema_name()
    }

    fn is_referenceable() -> bool {
        String::is_referenceable()
    }

    fn json_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        String::json_schema(generator)
    }
}

impl Deref for AstString {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl AsRef<str> for AstString {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for AstString {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for AstString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<&str> for AstString {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}

impl From<String> for AstString {
    fn from(value: String) -> Self {
        Self(value.into())
    }
}

impl From<CompactString> for AstString {
    fn from(value: CompactString) -> Self {
        Self(value)
    }
}

impl From<AstString> for CompactString {
    fn from(value: AstString) -> Self {
        value.0
    }
}

impl PartialEq<str> for AstString {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for AstString {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for AstString {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<AstString> for String {
    fn eq(&self, other: &AstString) -> bool {
        self == other.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_and_schema_match_string() {
        let value = AstString::from("name");
        assert_eq!(serde_json::to_value(&value).expect("serializes"), "name");
        assert_eq!(
            serde_json::to_value(schemars::schema_for!(AstString)).expect("schema serializes"),
            serde_json::to_value(schemars::schema_for!(String)).expect("schema serializes")
        );
    }
}
