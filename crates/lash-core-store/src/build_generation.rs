//! The deployment a drive request is pinned to, so the engine routes a replay
//! to a compatible build.
//!
//! The value is a digest: 12 lowercase hexadecimal characters, the first six
//! bytes of the build's drain-generation hash under ADR 0106 §1. Only
//! [`BuildGeneration::from_digest`], [`BuildGeneration::parse`] and
//! [`BuildGeneration::for_test`] mint one — there is no free-form
//! constructor, because a generation that is not a digest of the drain
//! formats names no build at all.
//!
//! The type lives in the store crate beside
//! [`crate::executable_generation::ExecutableGeneration`]: durable park and
//! admission records carry it, so it must sit below every writer that stamps
//! one (FIG-3795).

use lash_sansio::core_support::Blake3DomainHasher;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The drain generation of one build (FIG-3795).
#[derive(Clone, Debug, PartialEq, Eq, Hash, schemars::JsonSchema)]
pub struct BuildGeneration(String);

/// Why a string is not a [`BuildGeneration`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a build generation is 12 lowercase hexadecimal characters; got {0:?}")]
pub struct BuildGenerationParseError(String);

impl BuildGeneration {
    /// The generation whose rendering is the lowercase hex of `digest` — the
    /// first six bytes of the build's drain-generation hash.
    pub fn from_digest(digest: [u8; 6]) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut hex = String::with_capacity(12);
        for byte in digest {
            hex.push(HEX[(byte >> 4) as usize] as char);
            hex.push(HEX[(byte & 0x0f) as usize] as char);
        }
        Self(hex)
    }

    /// `text` as a generation, refusing anything that is not 12 lowercase
    /// hexadecimal characters.
    pub fn parse(text: &str) -> Result<Self, BuildGenerationParseError> {
        let valid = text.len() == 12
            && text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if valid {
            Ok(Self(text.to_owned()))
        } else {
            Err(BuildGenerationParseError(text.to_owned()))
        }
    }

    /// The generation as text: 12 lowercase hexadecimal characters.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The service-name suffix a generation-pinned lane carries: `"_g"` plus
    /// the hex, so `LashProcessWorkflow` is served beside
    /// `LashProcessWorkflow_g<hex>` (FIG-3795).
    pub fn service_suffix(&self) -> String {
        let mut suffix = String::with_capacity(14);
        suffix.push_str("_g");
        suffix.push_str(&self.0);
        suffix
    }

    /// A deterministic stand-in generation derived from `label`, so a test
    /// double can stand up two distinct builds in one process. Not a digest of
    /// anything a real build writes — only tests mint one.
    #[doc(hidden)]
    pub fn for_test(label: &'static str) -> Self {
        let mut hasher = Blake3DomainHasher::new("lash-build-generation/v1");
        hasher.update(b"for-test");
        hasher.update(label);
        let digest = hasher.finalize();
        let mut bytes = [0_u8; 6];
        bytes.copy_from_slice(&digest[..6]);
        Self::from_digest(bytes)
    }
}

impl std::fmt::Display for BuildGeneration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::str::FromStr for BuildGeneration {
    type Err = BuildGenerationParseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl Serialize for BuildGeneration {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for BuildGeneration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generation_renders_its_digest_as_twelve_lower_hex() {
        let generation = BuildGeneration::from_digest([0x3f, 0xa9, 0x00, 0xbc, 0x12, 0xde]);
        assert_eq!(generation.as_str(), "3fa900bc12de");
        assert_eq!(generation.to_string(), "3fa900bc12de");
        assert_eq!(
            BuildGeneration::parse("3fa900bc12de").expect("a rendered digest parses"),
            generation
        );
    }

    #[test]
    fn parse_refuses_anything_but_twelve_lower_hex() {
        for malformed in [
            "",
            "3fa900bc12d",
            "3fa900bc12de0",
            "3FA900BC12DE",
            "3fa900bc12dg",
            "t0",
        ] {
            assert!(
                BuildGeneration::parse(malformed).is_err(),
                "{malformed:?} is not a build generation"
            );
        }
    }

    #[test]
    fn the_service_suffix_is_g_plus_the_hex() {
        let generation = BuildGeneration::from_digest([0x3f, 0xa9, 0x00, 0xbc, 0x12, 0xde]);
        assert_eq!(generation.service_suffix(), "_g3fa900bc12de");
    }

    #[test]
    fn test_generations_are_valid_and_distinct_per_label() {
        let first = BuildGeneration::for_test("t0");
        let second = BuildGeneration::for_test("t1");
        assert_eq!(BuildGeneration::parse(first.as_str()).as_ref(), Ok(&first));
        assert_eq!(
            BuildGeneration::parse(second.as_str()).as_ref(),
            Ok(&second)
        );
        assert_ne!(first, second);
        assert_eq!(
            first,
            BuildGeneration::for_test("t0"),
            "for_test is deterministic"
        );
    }

    #[test]
    fn a_generation_stays_a_string_on_the_wire_but_validates_on_read() {
        let generation = BuildGeneration::for_test("t0");
        let encoded = serde_json::to_string(&generation).expect("serialize");
        assert_eq!(encoded, format!("\"{}\"", generation.as_str()));
        let decoded: BuildGeneration =
            serde_json::from_str(&encoded).expect("a serialized generation parses");
        assert_eq!(decoded, generation);
        assert!(serde_json::from_str::<BuildGeneration>("\"t0\"").is_err());
    }
}
