//! Deterministic key material for opening an agent frame.

use crate::core_support::Blake3DomainHasher;

const FRAME_KEY_PREFIX: &str = "frame-key/v2/";
const FRAME_KEY_VERSION: u8 = 2;

/// A non-empty, deterministically derived key that Lash turns into a durable
/// agent-frame identity.
///
/// Construct a key from either a stable tool call site with
/// [`FrameKey::from_call_site`] or explicit caller-owned material with
/// [`FrameKey::from_caller_material`]. Lash runtime compaction uses
/// [`FrameKey::from_compaction_material`]. There is deliberately no raw-string
/// constructor: callers name a frame, while Lash owns the resulting identity.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct FrameKey(String);

/// A caller attempted to derive an agent-frame key from invalid naming material.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKeyError {
    /// Caller-owned naming material must identify a frame rather than collapse
    /// unrelated callers onto the same blank-derived key.
    EmptyCallerMaterial,
}

impl std::fmt::Display for FrameKeyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyCallerMaterial => formatter
                .write_str("frame key caller material must not be empty or whitespace-only"),
        }
    }
}

impl std::error::Error for FrameKeyError {}

impl FrameKey {
    /// Derives a frame key for one tool call in one frame lineage.
    ///
    /// A redrive preserves all three inputs, while two distinct calls in the
    /// same frame have distinct `tool_call_id` values.
    pub fn from_call_site(session_id: &str, frame_lineage: &str, tool_call_id: &str) -> Self {
        Self::derive(0, [session_id, frame_lineage, tool_call_id])
    }

    /// Derives a frame key from explicit caller-owned naming material.
    pub fn from_caller_material(material: &str) -> Result<Self, FrameKeyError> {
        if material.trim().is_empty() {
            return Err(FrameKeyError::EmptyCallerMaterial);
        }
        Ok(Self::derive(1, [material]))
    }

    /// Derives the replay-stable key for a runtime compaction boundary.
    ///
    /// The three inputs are length-delimited independently so callers cannot
    /// assemble or ambiguously concatenate durable key bytes themselves.
    pub fn from_compaction_material(
        session_id: &str,
        boundary_id: &str,
        previous_frame_node_id: &str,
    ) -> Self {
        Self::derive(2, [session_id, boundary_id, previous_frame_node_id])
    }

    /// Returns the derived key consumed by Lash's durable frame-id derivation.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn derive<'a>(source_tag: u8, parts: impl IntoIterator<Item = &'a str>) -> Self {
        let mut digest = Blake3DomainHasher::new("lash.agent-frame-key/v2");
        digest.update([FRAME_KEY_VERSION, source_tag]);
        for part in parts {
            digest.update((part.len() as u64).to_be_bytes());
            digest.update(part.as_bytes());
        }
        Self(format!("{FRAME_KEY_PREFIX}{}", digest.finalize_hex()))
    }

    fn is_derived(value: &str) -> bool {
        value.strip_prefix(FRAME_KEY_PREFIX).is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
    }
}

impl std::fmt::Debug for FrameKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_tuple("FrameKey").field(&self.0).finish()
    }
}

impl serde::Serialize for FrameKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for FrameKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        if Self::is_derived(&value) {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom(
                "frame key must be derived by Lash from call-site or caller material",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_site_derivation_is_stable_and_call_specific() {
        let first = FrameKey::from_call_site("session", "frame", "call-1");
        assert_eq!(
            first,
            FrameKey::from_call_site("session", "frame", "call-1")
        );
        assert_eq!(
            first.as_str(),
            "frame-key/v2/4041d7061912b8be622d35c5c1cb10b2fe684e40395e2dd8c750c6feda56e58e"
        );
        assert_ne!(
            first,
            FrameKey::from_call_site("session", "frame", "call-2")
        );
    }

    #[test]
    fn caller_material_uses_the_same_derived_representation_in_its_own_domain() {
        let caller = FrameKey::from_caller_material("frame").expect("non-empty caller material");
        assert!(FrameKey::is_derived(caller.as_str()));
        assert_ne!(caller, FrameKey::from_call_site("", "", "frame"));
    }

    #[test]
    fn caller_material_rejects_empty_and_whitespace_with_a_typed_error() {
        assert_eq!(
            FrameKey::from_caller_material(""),
            Err(FrameKeyError::EmptyCallerMaterial)
        );
        assert_eq!(
            FrameKey::from_caller_material("  "),
            Err(FrameKeyError::EmptyCallerMaterial)
        );
    }

    #[test]
    fn caller_material_separates_callers_and_preserves_deliberate_reuse() {
        let first = FrameKey::from_caller_material("caller-one").expect("non-empty material");
        let second = FrameKey::from_caller_material("caller-two").expect("non-empty material");
        let reused = FrameKey::from_caller_material("caller-one").expect("non-empty material");

        assert_eq!(
            first.as_str(),
            "frame-key/v2/fe671c9a9b3ab71d456f809a0be6d1f9dbe3306aeda6af2881a30f836cba62b5"
        );
        assert_eq!(
            second.as_str(),
            "frame-key/v2/f14a7e6f82be497296c03890ec01dea6133116800a43cd5b30e691cc1a132413"
        );
        assert_eq!(
            reused.as_str(),
            "frame-key/v2/fe671c9a9b3ab71d456f809a0be6d1f9dbe3306aeda6af2881a30f836cba62b5"
        );
    }

    #[test]
    fn compaction_material_is_stable_and_boundary_specific() {
        let first = FrameKey::from_compaction_material("session", "turn", "frame-before");

        assert_eq!(
            first,
            FrameKey::from_compaction_material("session", "turn", "frame-before")
        );
        assert_eq!(
            first.as_str(),
            "frame-key/v2/c72af05c2980ca4fc9221f68a297a9706ee80c7058d82ba82ca1e7d8ff69a755"
        );
        assert_ne!(
            first,
            FrameKey::from_compaction_material("session", "turn", "frame-after")
        );
        assert!(FrameKey::is_derived(first.as_str()));
    }

    #[test]
    fn serde_rejects_raw_key_material() {
        let error = serde_json::from_value::<FrameKey>(serde_json::json!("raw-frame-id"))
            .expect_err("raw strings must not construct FrameKey");
        assert!(error.to_string().contains("must be derived by Lash"));

        let key = FrameKey::from_caller_material("named-frame").expect("non-empty caller material");
        let encoded = serde_json::to_value(&key).expect("serialize derived key");
        assert_eq!(
            serde_json::from_value::<FrameKey>(encoded).expect("deserialize derived key"),
            key
        );
    }
}
