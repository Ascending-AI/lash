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
/// for alone (`K-BND-003`). The asks are gathered and passed to the nursery
/// in batches of about [`BATCH`] bytes. A batch the nursery refuses is asked
/// again ask by ask, so the refusal names the same ask, the same held total
/// and the same wanted bytes as asking one by one would. Every hold, and the
/// end of the writing ([`Self::finish`]), settles the gathered asks first,
/// so no other event overtakes one of them.
pub(super) struct Text<'a, 'call> {
    text: String,
    new: &'a mut Nursery<'call>,
    /// Asks not yet passed to the nursery, in order, and their sum.
    asks: Vec<u64>,
    gathered: u64,
}

impl<'a, 'call> Text<'a, 'call> {
    pub fn new(new: &'a mut Nursery<'call>) -> Self {
        Self {
            text: String::new(),
            new,
            asks: Vec::new(),
            gathered: 0,
        }
    }

    /// The text written, once every gathered ask is settled. An ask the
    /// nursery refuses came before whatever ended `answer`, so it wins.
    pub fn finish<T>(mut self, answer: Result<T, NativeError>) -> Result<(T, String), NativeError> {
        self.settle()?;
        Ok((answer?, self.text))
    }

    fn ask(&mut self, bytes: u64) -> Result<(), NativeError> {
        if bytes == 0 {
            return Ok(());
        }
        self.asks.push(bytes);
        self.gathered = self.gathered.saturating_add(bytes);
        if self.gathered >= BATCH {
            self.settle()?;
        }
        Ok(())
    }

    fn settle(&mut self) -> Result<(), NativeError> {
        if self.asks.is_empty() {
            return Ok(());
        }
        let gathered = std::mem::take(&mut self.gathered);
        let mut asks = std::mem::take(&mut self.asks);
        let answer = if self.new.reserve(0, gathered).is_ok() {
            Ok(())
        } else {
            // The nursery held none of the batch: ask again one by one, so
            // the first ask that does not fit is refused as it would be.
            asks.iter()
                .try_for_each(|bytes| self.new.reserve(0, *bytes))
        };
        asks.clear();
        self.asks = asks;
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
        if bytes > IMMUTABLE_INLINE_MAX {
            self.settle()?;
        }
        self.new.read_whole(bytes)
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
