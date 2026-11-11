//! The codec writes the same pieces to a nursery-backed string or to a logical
//! byte counter. Counting does not reserve storage or create guest identities.
use lash_kernel_doc::heap::IMMUTABLE_INLINE_MAX;
use lash_kernel_doc::{NativeError, Nursery};

pub(super) trait Output {
    fn push(&mut self, piece: &str) -> Result<(), NativeError>;
    fn reserve(&mut self, bytes: u64) -> Result<(), NativeError>;
    fn read_whole(&mut self, bytes: u64) -> Result<(), NativeError>;
}

/// The reservation bytes a text output gathers before it asks the nursery.
const BATCH: u64 = 16 * 1024;

/// Text built in a nursery. Each piece costs twice its length and each
/// explicit reservation its bytes, in order, exactly as if each were asked
/// for alone (`K-BND-003`). The asks are summed and passed to the nursery
/// in batches of about [`BATCH`] bytes, each batch only while it fits the
/// room the nursery had left when the batch began. An ask that would not
/// fit settles the batch before it, which holds, and is then asked alone,
/// so its refusal names the same ask, the same held total and the same
/// wanted bytes as asking one by one would. Every hold, and the end of the
/// writing ([`Self::finish`]), settles the gathered asks first, so no other
/// event overtakes one of them.
pub(super) struct Text<'a, 'call> {
    text: String,
    new: &'a mut Nursery<'call>,
    /// The sum of the asks not yet passed to the nursery.
    gathered: u64,
    /// What the nursery had left when the gathering began.
    left: u64,
}

impl<'a, 'call> Text<'a, 'call> {
    pub fn new(new: &'a mut Nursery<'call>) -> Self {
        let left = new.left();
        Self {
            text: String::new(),
            new,
            gathered: 0,
            left,
        }
    }

    /// The text written, once every gathered ask is settled. An ask the
    /// nursery refuses came before whatever ended `answer`, so it wins.
    pub fn finish<T>(mut self, answer: Result<T, NativeError>) -> Result<(T, String), NativeError> {
        self.settle()?;
        Ok((answer?, self.text))
    }

    #[inline]
    fn ask(&mut self, bytes: u64) -> Result<(), NativeError> {
        // Both are at most `left`, so the sum cannot wrap.
        let gathered = self.gathered + bytes.min(u64::MAX / 2);
        if gathered >= BATCH.min(self.left) {
            return self.overflow(bytes);
        }
        self.gathered = gathered;
        Ok(())
    }

    /// An ask that ends the batch: one that fills it, or one that does not
    /// fit after the gathered asks, which all fit.
    #[cold]
    #[inline(never)]
    fn overflow(&mut self, bytes: u64) -> Result<(), NativeError> {
        if self.gathered.saturating_add(bytes) <= self.left {
            self.gathered += bytes;
            return self.settle();
        }
        self.settle()?;
        let answer = self.new.reserve(0, bytes);
        self.left = self.new.left();
        answer
    }

    fn settle(&mut self) -> Result<(), NativeError> {
        let gathered = std::mem::take(&mut self.gathered);
        let answer = if gathered == 0 {
            Ok(())
        } else {
            self.new.reserve(0, gathered)
        };
        self.left = self.new.left();
        answer
    }
}

impl Output for Text<'_, '_> {
    fn push(&mut self, piece: &str) -> Result<(), NativeError> {
        self.ask(super::room(piece.len()).saturating_mul(2))?;
        self.text.push_str(piece);
        #[cfg(test)]
        let _output = crate::native_contract_laws::scratch(self.text.len() as u64);
        Ok(())
    }
    fn reserve(&mut self, bytes: u64) -> Result<(), NativeError> {
        self.ask(bytes)
    }
    fn read_whole(&mut self, bytes: u64) -> Result<(), NativeError> {
        if bytes <= IMMUTABLE_INLINE_MAX {
            return self.new.read_whole(bytes);
        }
        self.settle()?;
        let answer = self.new.read_whole(bytes);
        self.left = self.new.left();
        answer
    }
}

#[derive(Default)]
pub(super) struct Count {
    pub bytes: usize,
}
impl Output for Count {
    fn push(&mut self, piece: &str) -> Result<(), NativeError> {
        self.bytes = self
            .bytes
            .checked_add(piece.len())
            .ok_or_else(crate::exhausted)?;
        Ok(())
    }
    fn reserve(&mut self, _: u64) -> Result<(), NativeError> {
        Ok(())
    }
    fn read_whole(&mut self, _: u64) -> Result<(), NativeError> {
        Ok(())
    }
}
