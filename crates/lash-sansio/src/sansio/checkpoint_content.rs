//! The content a [`TurnCheckpoint`] names but does not hold.
//!
//! A checkpoint keeps bounded machine state. The transcript it would
//! otherwise copy (the messages, the prompt view, the history events and a
//! pending model request) is stored content-addressed beside it, under
//! BLAKE3 digests of its canonical bytes, as the turn prelude is (FIG-5133).
//!
//! A sequence is a chain of chunks from its start. Each chunk names the
//! digest of the chunk before it, and the checkpoint names only the last. A
//! chunk closes once it holds [`CHUNK_ITEMS`] items or its items' encodings
//! reach [`CHUNK_BYTES`]; the last chunk stays open until it closes. The
//! chain is a function of the items alone, so appending to a sequence
//! rewrites only its open tail: every closed chunk before it keeps its
//! digest, and a store that already holds it keeps it once.
//!
//! A [`Chain`] is that structure as a store holds it: its closed chunks by
//! digest and the encodings of its open tail's items. The next checkpoint of
//! the same sequence builds on it (FIG-5955): it encodes only the items past
//! what it keeps, re-joins the tail from the encodings it holds, and names
//! every closed chunk it keeps without reading or writing it again.
//!
//! [`TurnCheckpoint`]: super::TurnCheckpoint

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;

use super::TurnCheckpointRestoreError;

/// version_surface = "coexist"
/// version_guard(items(TURN_CHECKPOINT_CONTENT_DOMAIN, CHUNK_ITEMS, CHUNK_BYTES, Chunk, CheckpointContentRef))
const TURN_CHECKPOINT_CONTENT_DOMAIN: &str = "lash-turn-checkpoint-content/v1";

/// The most items a chunk of a sequence chain holds.
const CHUNK_ITEMS: usize = 32;

/// The item bytes past which a chunk closes: what bounds the open tail a
/// checkpoint re-joins and re-stores.
const CHUNK_BYTES: usize = 4 * 1024;

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

    /// A digest as a store recorded it: what
    /// [`TurnCheckpointContent::insert_stored`] checks the blob it names
    /// against.
    #[must_use]
    pub fn stored(digest: String) -> Self {
        Self(digest)
    }
}

impl std::fmt::Display for CheckpointContentRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The host's name for a content blob it stored, encoded by the host: the
/// machine only keeps it, so a later checkpoint can name the blob without
/// its bytes ([`TurnMachine::checkpoint_delta`](super::TurnMachine::checkpoint_delta)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CheckpointContentPin([u8; 32]);

impl CheckpointContentPin {
    #[must_use]
    pub const fn new(pin: [u8; 32]) -> Self {
        Self(pin)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
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

    /// Whether a blob is held under `digest`.
    #[must_use]
    pub fn contains(&self, digest: &CheckpointContentRef) -> bool {
        self.0.contains_key(digest)
    }

    /// Keep only the blobs `keep` answers `true` for.
    pub(super) fn retain(&mut self, mut keep: impl FnMut(&CheckpointContentRef) -> bool) {
        self.0.retain(|digest, _| keep(digest));
    }

    /// The items of the chain whose last chunk is `head`, in order, with the
    /// chain as it is stored.
    pub(super) fn sequence<T: serde::Serialize + DeserializeOwned>(
        &self,
        head: &CheckpointContentRef,
    ) -> Result<(Vec<T>, Chain), TurnCheckpointRestoreError> {
        let mut chunks = Vec::new();
        let mut next = Some(head.clone());
        while let Some(digest) = next {
            if chunks.len() > self.0.len() {
                return Err(TurnCheckpointRestoreError::CorruptContent { content: digest.0 });
            }
            let chunk: Chunk<T> = self.value(&digest)?;
            next = chunk.prev;
            chunks.push((digest, chunk.items));
        }
        chunks.reverse();
        let mut closed = Vec::new();
        let mut items = Vec::new();
        let mut tail = Vec::new();
        let count = chunks.len();
        for (index, (digest, chunk)) in chunks.into_iter().enumerate() {
            // Every chunk but the last closed; the last is open unless its
            // items close it.
            if index + 1 == count {
                tail = chunk.iter().map(encode).collect();
                if closes(&tail) {
                    tail.clear();
                } else {
                    items.extend(chunk);
                    continue;
                }
            }
            items.extend(chunk);
            closed.push((digest, items.len()));
        }
        let chain = Chain {
            closed,
            tail,
            head: head.clone(),
        };
        Ok((items, chain))
    }
}

/// A sequence chain as a store holds it: its closed chunks, each with the
/// count of items through it, the encodings of the items of its open tail,
/// and its last chunk's digest, which names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Chain {
    closed: Vec<(CheckpointContentRef, usize)>,
    tail: Vec<String>,
    head: CheckpointContentRef,
}

impl Chain {
    /// The digest of the chain's last chunk: what a checkpoint names.
    pub(super) fn head(&self) -> &CheckpointContentRef {
        &self.head
    }

    /// The items the chain holds.
    pub(super) fn len(&self) -> usize {
        self.closed.last().map_or(0, |(_, end)| *end) + self.tail.len()
    }

    /// Every blob the chain is stored as.
    pub(super) fn blobs(&self) -> impl Iterator<Item = &CheckpointContentRef> {
        let open = (self.closed.last().map(|(digest, _)| digest) != Some(&self.head))
            .then_some(&self.head);
        self.closed.iter().map(|(digest, _)| digest).chain(open)
    }

    /// The chain of `items`, the `len` items of a sequence, building on
    /// `base`, the stored chain of an earlier state of the same sequence
    /// whose first `stable` items are still these: a closed chunk wholly
    /// among them is kept as it is stored, and the tail's encodings among
    /// them are re-joined; every other item is encoded, counted in
    /// `encoded`. Each chunk the chain does not keep is put into `content`.
    pub(super) fn of<'a, T: serde::Serialize + 'a>(
        items: impl Iterator<Item = &'a T>,
        len: usize,
        base: Option<&Chain>,
        stable: usize,
        content: &mut TurnCheckpointContent,
        encoded: &mut usize,
    ) -> Self {
        let empty = Chain {
            closed: Vec::new(),
            tail: Vec::new(),
            head: CheckpointContentRef::of_bytes(chunk_text(None, &[]).as_bytes()),
        };
        let base = base.unwrap_or(&empty);
        let stable = stable.min(base.len()).min(len);
        let kept = base.closed.partition_point(|(_, end)| *end <= stable);
        let mut closed = base.closed[..kept].to_vec();
        let start = closed.last().map_or(0, |(_, end)| *end);
        let mut open = if kept == base.closed.len() {
            base.tail[..stable - start].to_vec()
        } else {
            Vec::new()
        };
        let mut prev = closed.last().map(|(digest, _)| digest.clone());
        for item in items.skip(start + open.len()) {
            open.push(encode(item));
            *encoded += 1;
            if closes(&open) {
                let digest = content.put(chunk_text(prev.as_ref(), &open));
                closed.push((digest.clone(), start_of(&closed) + open.len()));
                prev = Some(digest);
                open.clear();
            }
        }
        let head = match closed.last() {
            Some((digest, _)) if open.is_empty() => digest.clone(),
            _ => content.put(chunk_text(prev.as_ref(), &open)),
        };
        Self {
            closed,
            tail: open,
            head,
        }
    }
}

/// The items through the last of `closed`.
fn start_of(closed: &[(CheckpointContentRef, usize)]) -> usize {
    closed.last().map_or(0, |(_, end)| *end)
}

/// Whether a chunk of items encoded as `items` is closed.
fn closes(items: &[String]) -> bool {
    items.len() >= CHUNK_ITEMS || items.iter().map(String::len).sum::<usize>() >= CHUNK_BYTES
}

/// A chunk's canonical text from its items' encodings: the bytes
/// [`encode`] makes of the [`Chunk`] holding them.
fn chunk_text(prev: Option<&CheckpointContentRef>, items: &[String]) -> String {
    let mut text =
        String::with_capacity(96 + items.len() + items.iter().map(String::len).sum::<usize>());
    text.push_str("{\"prev\":");
    match prev {
        Some(digest) => {
            text.push('"');
            text.push_str(digest.as_str());
            text.push('"');
        }
        None => text.push_str("null"),
    }
    text.push_str(",\"items\":[");
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            text.push(',');
        }
        text.push_str(item);
    }
    text.push_str("]}");
    text
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
