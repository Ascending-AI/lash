use super::{Arc, TokenCodec, TokenError, TokenReader, TokenWriter};

// Keys encode as byte tokens. Short keys own their bytes inline; path
// copies of longer keys share the same immutable backing allocation.

#[derive(Clone, Debug)]
pub struct Key(KeyBytes);
#[derive(Clone, Debug)]
enum KeyBytes {
    Inline { len: u8, bytes: [u8; 32] },
    Shared(Arc<[u8]>),
}

impl From<&[u8]> for Key {
    fn from(value: &[u8]) -> Self {
        if value.len() <= 32 {
            let mut bytes = [0; 32];
            bytes[..value.len()].copy_from_slice(value);
            Self(KeyBytes::Inline {
                len: value.len() as u8,
                bytes,
            })
        } else {
            Self(KeyBytes::Shared(Arc::from(value)))
        }
    }
}
impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}
impl Eq for Key {}
impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Key {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_slice().cmp(other.as_slice())
    }
}
impl Key {
    pub fn as_slice(&self) -> &[u8] {
        match &self.0 {
            KeyBytes::Inline { len, bytes } => &bytes[..usize::from(*len)],
            KeyBytes::Shared(bytes) => bytes,
        }
    }
    pub(super) fn len(&self) -> usize {
        self.as_slice().len()
    }
    pub(super) fn heap_bytes(&self) -> usize {
        match &self.0 {
            KeyBytes::Inline { .. } => 0,
            KeyBytes::Shared(bytes) => bytes.len() + 16,
        }
    }
}
impl TokenCodec for Key {
    fn encode(&self, out: &mut TokenWriter) {
        out.bytes(self.as_slice());
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.bytes().map(Self::from)
    }
}

impl From<Vec<u8>> for Key {
    fn from(bytes: Vec<u8>) -> Self {
        Self::from(bytes.as_slice())
    }
}
impl<const N: usize> From<[u8; N]> for Key {
    fn from(bytes: [u8; N]) -> Self {
        Self::from(bytes.as_slice())
    }
}
impl std::ops::Deref for Key {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}
impl AsRef<[u8]> for Key {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}
