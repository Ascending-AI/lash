//! Explicit bounds on staged encoding memory.

use crate::GraphRefusal;

/// A limit a host chose. It has no default: every budget names its numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExplicitLimit(u64);

impl ExplicitLimit {
    #[must_use]
    pub const fn new(limit: u64) -> Self {
        Self(limit)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// An encoder's memory lease: every staged byte is charged before it is
/// held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeBudget {
    lease: ExplicitLimit,
    spent: u64,
}

impl EncodeBudget {
    #[must_use]
    pub const fn new(lease: ExplicitLimit) -> Self {
        Self { lease, spent: 0 }
    }

    /// Charges `bytes`, refusing when the lease would be exceeded. A refused
    /// charge spends nothing.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::EncodeMemoryLimit`].
    pub fn charge(&mut self, bytes: u64) -> Result<(), GraphRefusal> {
        let spent = self
            .spent
            .checked_add(bytes)
            .filter(|spent| *spent <= self.lease.get())
            .ok_or(GraphRefusal::EncodeMemoryLimit)?;
        self.spent = spent;
        Ok(())
    }

    #[must_use]
    pub const fn spent(&self) -> u64 {
        self.spent
    }

    #[must_use]
    pub const fn lease(&self) -> ExplicitLimit {
        self.lease
    }
}
