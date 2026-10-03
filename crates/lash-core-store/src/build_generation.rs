//! The deployment a shift request is pinned to, so the engine routes a replay
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

/// version_surface = "coexist"
/// version_guard(items(LASH_BUILD_GENERATION_DOMAIN_VERSION, for_test))
const LASH_BUILD_GENERATION_DOMAIN_VERSION: &str = "lash-build-generation/v1";

use lash_sansio::core_support::Blake3DomainHasher;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The drain generation of one build (FIG-3795).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, schemars::JsonSchema)]
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
        let mut hasher = Blake3DomainHasher::new(LASH_BUILD_GENERATION_DOMAIN_VERSION);
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

/// The generation an engine runs on, and how it comes to hold one.
///
/// A build's generation folds in its ordered plugin composition, which exists
/// only once the core's plugins are registered, and the engine is built
/// before that. So an engine holds this slot: the core binds the generation
/// it computed after registration, once, and every reader takes it from here.
/// Until then there is no generation to read, so no work can be stamped with
/// one that no deployment serves.
///
/// A test double fixes its generation at construction instead
/// ([`Self::fixed`]): the stamp stands in for a composition, and the core's
/// bind leaves it as it is.
#[derive(Clone, Debug)]
pub struct EngineGeneration {
    slot: std::sync::Arc<std::sync::OnceLock<BuildGeneration>>,
    fixed: bool,
}

/// An engine's generation was read before a core bound it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "the engine has no build generation yet: a core binds it when it is built over the engine's backend"
)]
pub struct GenerationUnbound;

/// A core computed a generation other than the one its engine is already
/// bound to: another core, with another plugin composition, was built over
/// the same engine.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "the engine already runs build generation `{bound}`; this core's plugin composition is generation `{composed}`"
)]
pub struct GenerationRebound {
    pub bound: BuildGeneration,
    pub composed: BuildGeneration,
}

impl EngineGeneration {
    /// A slot the core binds after its plugins are registered.
    pub fn unbound() -> Self {
        Self {
            slot: std::sync::Arc::default(),
            fixed: false,
        }
    }

    /// A slot holding `generation` from the start: a test double's stamp, or
    /// a handle on a generation a core already bound.
    pub fn fixed(generation: BuildGeneration) -> Self {
        Self {
            slot: std::sync::Arc::new(std::sync::OnceLock::from(generation)),
            fixed: true,
        }
    }

    /// The engine's generation.
    ///
    /// # Errors
    /// [`GenerationUnbound`] until a core has bound it.
    pub fn get(&self) -> Result<&BuildGeneration, GenerationUnbound> {
        self.slot.get().ok_or(GenerationUnbound)
    }

    /// Bind the generation a core computed after registration. Binding the
    /// same generation again is a no-op, so several cores of one composition
    /// share an engine.
    ///
    /// # Errors
    /// [`GenerationRebound`] when the slot is already bound to another
    /// generation.
    pub fn bind(&self, composed: &BuildGeneration) -> Result<(), GenerationRebound> {
        if self.fixed {
            return Ok(());
        }
        let bound = self.slot.get_or_init(|| composed.clone());
        if bound == composed {
            Ok(())
        } else {
            Err(GenerationRebound {
                bound: bound.clone(),
                composed: composed.clone(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_generation_stays_a_string_on_the_wire_but_validates_on_read() {
        let generation = BuildGeneration::for_test("t0");
        let encoded = serde_json::to_string(&generation).expect("serialize");
        assert_eq!(encoded, format!("\"{}\"", generation.as_str()));
        let decoded: BuildGeneration =
            serde_json::from_str(&encoded).expect("a serialized generation parses");
        assert_eq!(decoded, generation);
        assert!(serde_json::from_str::<BuildGeneration>("\"t0\"").is_err());
    }

    #[test]
    fn an_unbound_slot_is_bound_once_and_refuses_another_generation() {
        let slot = EngineGeneration::unbound();
        assert_eq!(slot.get(), Err(GenerationUnbound));
        let first = BuildGeneration::for_test("t0");
        let second = BuildGeneration::for_test("t1");
        slot.bind(&first).expect("the first bind");
        slot.bind(&first).expect("the same generation again");
        assert_eq!(slot.clone().get(), Ok(&first));
        assert_eq!(
            slot.bind(&second),
            Err(GenerationRebound {
                bound: first.clone(),
                composed: second.clone(),
            })
        );
        let fixed = EngineGeneration::fixed(first.clone());
        fixed.bind(&second).expect("a fixed stamp stands in");
        assert_eq!(fixed.get(), Ok(&first));
    }
}
