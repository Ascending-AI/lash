//! The content a [`TurnCheckpoint`] names but does not hold.
//!
//! A checkpoint keeps bounded machine state. The transcript it would
//! otherwise copy (the messages, the prompt view, the history events and a
//! pending model request) is stored content-addressed beside it, under
//! BLAKE3 digests of its canonical bytes, as the turn prelude is (FIG-5133).
//!
//! A sequence is a chain of fixed-size chunks from its start. Each chunk
//! names the digest of the chunk before it, and the checkpoint names only the
//! last. The chain is a function of the items alone, so appending to a
//! sequence rewrites only its tail chunk: every full chunk before it keeps its
//! digest, and a store that already holds it keeps it once.
//!
//! [`TurnCheckpoint`]: super::TurnCheckpoint

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;

use super::TurnCheckpointRestoreError;

/// version_surface = "coexist"
/// version_guard(items(TURN_CHECKPOINT_CONTENT_DOMAIN, CHUNK_ITEMS, Chunk, CheckpointContentRef))
const TURN_CHECKPOINT_CONTENT_DOMAIN: &str = "lash-turn-checkpoint-content/v1";

/// Items per chunk of a sequence chain.
const CHUNK_ITEMS: usize = 32;

/// The digest of one content blob's bytes.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct CheckpointContentRef(String);

impl CheckpointContentRef {
    pub(super) fn of_bytes(bytes: &[u8]) -> Self {
        Self(crate::core_support::blake3_domain_hash_hex(
            TURN_CHECKPOINT_CONTENT_DOMAIN,
            bytes,
        ))
    }

    /// The digest as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CheckpointContentRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One chunk of a sequence chain.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Chunk<T> {
    prev: Option<CheckpointContentRef>,
    items: Vec<T>,
}

/// Content-addressed blobs a checkpoint names, each the canonical JSON of
/// one chunk or value under its [`CheckpointContentRef`]. A host stores each blob it
/// does not already hold beside the checkpoint row, in the same transaction.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct TurnCheckpointContent(BTreeMap<CheckpointContentRef, String>);

impl TurnCheckpointContent {
    /// Every blob, by digest.
    pub fn iter(&self) -> impl Iterator<Item = (&CheckpointContentRef, &[u8])> {
        self.0
            .iter()
            .map(|(digest, text)| (digest, text.as_bytes()))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Total bytes over every blob.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.0.values().map(String::len).sum()
    }

    /// Add a blob read back from a store, refusing bytes that are not the
    /// ones `digest` names.
    ///
    /// # Errors
    ///
    /// [`TurnCheckpointRestoreError::CorruptContent`].
    pub fn insert_stored(
        &mut self,
        digest: CheckpointContentRef,
        bytes: Vec<u8>,
    ) -> Result<(), TurnCheckpointRestoreError> {
        if CheckpointContentRef::of_bytes(&bytes) != digest {
            return Err(TurnCheckpointRestoreError::CorruptContent { content: digest.0 });
        }
        let text =
            String::from_utf8(bytes).map_err(|_| TurnCheckpointRestoreError::CorruptContent {
                content: digest.0.clone(),
            })?;
        self.0.insert(digest, text);
        Ok(())
    }

    fn put(&mut self, text: String) -> CheckpointContentRef {
        let digest = CheckpointContentRef::of_bytes(text.as_bytes());
        self.0.entry(digest.clone()).or_insert(text);
        digest
    }

    fn get(&self, digest: &CheckpointContentRef) -> Result<&str, TurnCheckpointRestoreError> {
        let text =
            self.0
                .get(digest)
                .ok_or_else(|| TurnCheckpointRestoreError::MissingContent {
                    content: digest.0.clone(),
                })?;
        if CheckpointContentRef::of_bytes(text.as_bytes()) != *digest {
            return Err(TurnCheckpointRestoreError::CorruptContent {
                content: digest.0.clone(),
            });
        }
        Ok(text)
    }

    /// Store `value` as one blob.
    pub(super) fn put_value<T: serde::Serialize>(&mut self, value: &T) -> CheckpointContentRef {
        self.put(encode(value))
    }

    /// The value one blob holds.
    pub(super) fn value<T: DeserializeOwned>(
        &self,
        digest: &CheckpointContentRef,
    ) -> Result<T, TurnCheckpointRestoreError> {
        decode(digest, self.get(digest)?)
    }

    /// Store `items` as a chunk chain and answer the digest of its last
    /// chunk. An empty sequence is one empty chunk.
    pub(super) fn put_sequence<'a, T: serde::Serialize + 'a>(
        &mut self,
        items: impl IntoIterator<Item = &'a T>,
    ) -> CheckpointContentRef {
        let items = items.into_iter().collect::<Vec<_>>();
        let mut chunks = items.chunks(CHUNK_ITEMS);
        let first = chunks.next().unwrap_or_default();
        let mut head = self.put_value(&Chunk {
            prev: None,
            items: first.to_vec(),
        });
        for chunk in chunks {
            head = self.put_value(&Chunk {
                prev: Some(head),
                items: chunk.to_vec(),
            });
        }
        head
    }

    /// The items of the chain whose last chunk is `head`, in order.
    pub(super) fn sequence<T: DeserializeOwned>(
        &self,
        head: &CheckpointContentRef,
    ) -> Result<Vec<T>, TurnCheckpointRestoreError> {
        let mut chunks = Vec::new();
        let mut next = Some(head.clone());
        while let Some(digest) = next {
            if chunks.len() > self.0.len() {
                return Err(TurnCheckpointRestoreError::CorruptContent { content: digest.0 });
            }
            let chunk: Chunk<T> = self.value(&digest)?;
            next = chunk.prev;
            chunks.push(chunk.items);
        }
        Ok(chunks.into_iter().rev().flatten().collect())
    }
}

#[expect(
    clippy::expect_used,
    reason = "checkpoint content is the machine's own serde-derived state, which the checkpoint body already encodes as JSON; a failure means a type was widened past JSON, which the message names"
)]
fn encode<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("checkpoint content must encode as JSON")
}

fn decode<T: DeserializeOwned>(
    digest: &CheckpointContentRef,
    text: &str,
) -> Result<T, TurnCheckpointRestoreError> {
    serde_json::from_str(text).map_err(|error| TurnCheckpointRestoreError::IncompatibleFormat {
        message: format!("checkpoint content `{digest}`: {error}"),
    })
}
