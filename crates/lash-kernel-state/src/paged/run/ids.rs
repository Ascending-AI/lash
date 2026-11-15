//! Object ids (cutover contract §6): a u64 minted from the high-water mark
//! each run root persists, never reused within a lineage.

use lash_kernel_value::tokens::{TokenCodec, TokenError, TokenReader, TokenWriter};

/// A written object's identity, meaningful only under the root it was read
/// through: sibling forks that mint the same number never alias, as each
/// resident heap is opened from one root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId(pub u64);

impl ObjectId {
    /// The id's directory key: big-endian, so key order is id order.
    pub fn key(self) -> [u8; 8] {
        self.0.to_be_bytes()
    }

    /// The id a directory key names.
    pub fn of_key(key: &[u8]) -> Option<Self> {
        Some(Self(u64::from_be_bytes(key.try_into().ok()?)))
    }
}

impl TokenCodec for ObjectId {
    fn encode(&self, out: &mut TokenWriter) {
        out.unsigned(self.0);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.unsigned().map(Self)
    }
}

/// The next id a lineage mints. Every id below it has been minted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct HighWater(pub u64);

/// The lineage has minted every id: no write may take another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the run has minted every object id")]
pub struct IdsExhausted;

impl HighWater {
    /// Mints the next id. The last u64 is never minted: the mark past it
    /// could not be persisted.
    ///
    /// # Errors
    ///
    /// [`IdsExhausted`] at `u64::MAX`.
    pub fn mint(&mut self) -> Result<ObjectId, IdsExhausted> {
        if self.0 == u64::MAX {
            return Err(IdsExhausted);
        }
        let id = ObjectId(self.0);
        self.0 += 1;
        Ok(id)
    }

    /// Whether `id` was minted under this mark.
    pub fn minted(self, id: ObjectId) -> bool {
        id.0 < self.0
    }
}

impl TokenCodec for HighWater {
    fn encode(&self, out: &mut TokenWriter) {
        out.unsigned(self.0);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.unsigned().map(Self)
    }
}
