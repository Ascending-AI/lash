//! The one spelling of a process definition id, and the one record that
//! carries it.
//!
//! A process definition is immutable content (ADR 0095, amended by FIG-4174).
//! Its id is derived from that content, so two equal canonical definitions
//! share one id and any changed byte names a different definition. The id
//! names content only: it grants no authority and retains nothing. A host pin,
//! a frame, a process record or another durable referrer retains a
//! definition's artifacts (ADR 0113); a copied id is data.
//!
//! The canonical spelling is
//!
//! ```text
//! lash.definition:sha256:<64 lowercase hexadecimal digits>
//! ```
//!
//! and wherever an id sits inside a JSON value — a model value, a process
//! argument or result, a journal entry, a remote DTO — it is the tagged record
//!
//! ```json
//! { "$lash_definition_id": "lash.definition:sha256:…" }
//! ```
//!
//! never a bare string, so a walker over any value can tell an id from text
//! that happens to look like one. [`ProcessDefinitionId`]'s serde *is* that
//! record, and its [`Display`](std::fmt::Display) is the spelling a SQL column
//! or a log line holds. There is no other encoding.
//!
//! This module owns the spelling. The digest's preimage belongs to the
//! definition descriptor in `lash-core-execution`, which is the only caller of
//! [`ProcessDefinitionId::from_sha256_digest`]. It lives in `lash-sansio` for
//! the reason the handle codec does: the language runtime, core, the remote
//! protocol and hosts all read ids, and none of them may depend on another for
//! the format.

use serde::{Deserialize, Serialize};

/// The field that makes a JSON record a definition id.
pub const DEFINITION_ID_FIELD: &str = "$lash_definition_id";

/// Everything before the digest in a definition id's spelling.
pub const DEFINITION_ID_PREFIX: &str = "lash.definition:sha256:";

const DEFINITION_ID_HEX_LEN: usize = 64;

/// The content-derived id of one immutable process definition.
///
/// Serializes as `{"$lash_definition_id": "<spelling>"}` and refuses any other
/// field on the way in. Ordering is over the spelling.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProcessDefinitionId(String);

/// Why a spelling or a record is not a definition id.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvalidProcessDefinitionId {
    /// The text is not `lash.definition:sha256:` followed by 64 lowercase
    /// hexadecimal digits.
    Spelling { value: String },
    /// The JSON value is not an object whose one field is
    /// [`DEFINITION_ID_FIELD`] holding a string.
    NotTagged,
}

impl std::fmt::Display for InvalidProcessDefinitionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spelling { value } => write!(
                formatter,
                "`{}` is not a process definition id: a definition id is \
                 `{DEFINITION_ID_PREFIX}` followed by {DEFINITION_ID_HEX_LEN} lowercase hex digits",
                value.escape_debug()
            ),
            Self::NotTagged => write!(
                formatter,
                "a process definition id is the record `{{\"{DEFINITION_ID_FIELD}\": \"<id>\"}}` \
                 with no other field"
            ),
        }
    }
}

impl std::error::Error for InvalidProcessDefinitionId {}

impl ProcessDefinitionId {
    /// The id whose digest is `digest`.
    ///
    /// The definition descriptor's derivation is the only caller that turns
    /// content into an id; anything else holding an id parsed it.
    pub fn from_sha256_digest(digest: [u8; 32]) -> Self {
        let mut spelling =
            String::with_capacity(DEFINITION_ID_PREFIX.len() + DEFINITION_ID_HEX_LEN);
        spelling.push_str(DEFINITION_ID_PREFIX);
        for byte in digest {
            spelling.push_str(&format!("{byte:02x}"));
        }
        Self(spelling)
    }

    /// Parses the canonical spelling.
    ///
    /// # Errors
    ///
    /// [`InvalidProcessDefinitionId::Spelling`] for any other text, including
    /// uppercase hex, a different algorithm and a digest of the wrong length.
    pub fn parse(value: &str) -> Result<Self, InvalidProcessDefinitionId> {
        let valid = value.strip_prefix(DEFINITION_ID_PREFIX).is_some_and(|hex| {
            hex.len() == DEFINITION_ID_HEX_LEN
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
        if valid {
            Ok(Self(value.to_string()))
        } else {
            Err(InvalidProcessDefinitionId::Spelling {
                value: value.to_string(),
            })
        }
    }

    /// The canonical spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The tagged record, `{"$lash_definition_id": "<spelling>"}`.
    pub fn to_tagged_json(&self) -> serde_json::Value {
        serde_json::json!({ DEFINITION_ID_FIELD: self.0 })
    }

    /// Reads the tagged record.
    ///
    /// # Errors
    ///
    /// [`InvalidProcessDefinitionId::NotTagged`] unless `value` is an object
    /// with exactly the one field, and
    /// [`InvalidProcessDefinitionId::Spelling`] if that field's text is not an
    /// id.
    pub fn from_tagged_json(value: &serde_json::Value) -> Result<Self, InvalidProcessDefinitionId> {
        let record = value
            .as_object()
            .filter(|record| record.len() == 1)
            .ok_or(InvalidProcessDefinitionId::NotTagged)?;
        let spelling = record
            .get(DEFINITION_ID_FIELD)
            .and_then(serde_json::Value::as_str)
            .ok_or(InvalidProcessDefinitionId::NotTagged)?;
        Self::parse(spelling)
    }
}

impl std::fmt::Display for ProcessDefinitionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::str::FromStr for ProcessDefinitionId {
    type Err = InvalidProcessDefinitionId;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<&str> for ProcessDefinitionId {
    type Error = InvalidProcessDefinitionId;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

/// The serde form of the tagged record: one field, nothing else.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaggedDefinitionId<Spelling> {
    #[serde(rename = "$lash_definition_id")]
    spelling: Spelling,
}

impl Serialize for ProcessDefinitionId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        TaggedDefinitionId {
            spelling: self.as_str(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ProcessDefinitionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let tagged = TaggedDefinitionId::<String>::deserialize(deserializer)?;
        Self::parse(&tagged.spelling).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPELLING: &str =
        "lash.definition:sha256:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn digest() -> [u8; 32] {
        let mut digest = [0_u8; 32];
        for (index, byte) in digest.iter_mut().enumerate() {
            *byte = u8::try_from((index % 16) * 0x11).expect("fits a byte");
        }
        digest
    }

    #[test]
    fn serde_display_and_tagged_record_share_one_spelling() {
        let id = ProcessDefinitionId::from_sha256_digest(digest());
        assert_eq!(id.as_str(), SPELLING);
        assert_eq!(id.to_string(), SPELLING);
        assert_eq!(ProcessDefinitionId::parse(SPELLING), Ok(id.clone()));

        let tagged = serde_json::json!({ "$lash_definition_id": SPELLING });
        assert_eq!(serde_json::to_value(&id).expect("serialize id"), tagged);
        assert_eq!(id.to_tagged_json(), tagged);
        assert_eq!(
            ProcessDefinitionId::from_tagged_json(&tagged),
            Ok(id.clone())
        );
        assert_eq!(
            serde_json::from_value::<ProcessDefinitionId>(tagged).expect("deserialize id"),
            id
        );
    }

    #[test]
    fn other_spellings_are_refused() {
        let hex = &SPELLING[DEFINITION_ID_PREFIX.len()..];
        for value in [
            String::new(),
            hex.to_string(),
            format!("{DEFINITION_ID_PREFIX}{}", hex.to_uppercase()),
            format!("{DEFINITION_ID_PREFIX}{}", &hex[1..]),
            format!("{DEFINITION_ID_PREFIX}{hex}0"),
            format!("lash.definition:blake3:{hex}"),
            format!("{DEFINITION_ID_PREFIX}{}g", &hex[1..]),
        ] {
            assert_eq!(
                ProcessDefinitionId::parse(&value),
                Err(InvalidProcessDefinitionId::Spelling {
                    value: value.clone()
                }),
                "{value:?}"
            );
        }
    }

    #[test]
    fn a_bare_string_or_an_extra_field_is_not_an_id() {
        let bare = serde_json::json!(SPELLING);
        let extra = serde_json::json!({ "$lash_definition_id": SPELLING, "name": "scan" });
        let empty = serde_json::json!({});
        for value in [&bare, &extra, &empty] {
            assert_eq!(
                ProcessDefinitionId::from_tagged_json(value),
                Err(InvalidProcessDefinitionId::NotTagged)
            );
            assert!(serde_json::from_value::<ProcessDefinitionId>(value.clone()).is_err());
        }
        let malformed = serde_json::json!({ "$lash_definition_id": "lash.definition:sha256:ab" });
        assert!(matches!(
            ProcessDefinitionId::from_tagged_json(&malformed),
            Err(InvalidProcessDefinitionId::Spelling { .. })
        ));
        assert!(serde_json::from_value::<ProcessDefinitionId>(malformed).is_err());
    }
}
