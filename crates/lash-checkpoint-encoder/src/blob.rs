//! The content identity shared by blobs and graph nodes.

/// version_surface = "coexist"
/// version_guard(items(LASH_BLOB_DOMAIN_VERSION), items(for_content))
const LASH_BLOB_DOMAIN_VERSION: &str = "lash-blob/v2";

#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct BlobRef(pub String);

impl BlobRef {
    pub fn for_content(content: &[u8]) -> Self {
        Self::for_content_parts([content])
    }

    /// Hash borrowed consecutive slices under the blob identity domain.
    pub fn for_content_parts<'a>(parts: impl IntoIterator<Item = &'a [u8]>) -> Self {
        let mut hasher = blob_hasher();
        for part in parts {
            hasher.update(part);
        }
        Self(hasher.finalize().to_hex().to_string())
    }

    /// Raw digest under the same domain as [`Self::for_content`].
    pub fn digest_for_content(content: &[u8]) -> [u8; 32] {
        let mut hasher = blob_hasher();
        hasher.update(content);
        hasher.finalize().into()
    }

    /// Exposes the opaque durable blob reference to store implementors for backend round-tripping
    /// without imposing path or URL semantics.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BlobRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for BlobRef {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// The domain is unchanged: blob and graph identities share one preimage.
fn blob_hasher() -> blake3::Hasher {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(LASH_BLOB_DOMAIN_VERSION.len() as u64).to_be_bytes());
    hasher.update(LASH_BLOB_DOMAIN_VERSION.as_bytes());
    hasher
}
