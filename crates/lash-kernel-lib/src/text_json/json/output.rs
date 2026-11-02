//! The codec writes the same pieces to a nursery-backed string or to a logical
//! byte counter. Counting does not reserve storage or create guest identities.
use lash_kernel_doc::{NativeError, Nursery};

pub(super) trait Output {
    fn push(&mut self, piece: &str) -> Result<(), NativeError>;
    fn reserve(&mut self, bytes: u64) -> Result<(), NativeError>;
    fn read_whole(&mut self, bytes: u64) -> Result<(), NativeError>;
}

pub(super) struct Text<'a, 'call> {
    pub text: String,
    pub new: &'a mut Nursery<'call>,
}
impl Output for Text<'_, '_> {
    fn push(&mut self, piece: &str) -> Result<(), NativeError> {
        self.new
            .reserve(0, super::room(piece.len()).saturating_mul(2))?;
        self.text.push_str(piece);
        #[cfg(test)]
        let _output = crate::native_contract_laws::scratch(self.text.len() as u64);
        Ok(())
    }
    fn reserve(&mut self, bytes: u64) -> Result<(), NativeError> {
        self.new.reserve(0, bytes)
    }
    fn read_whole(&mut self, bytes: u64) -> Result<(), NativeError> {
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
