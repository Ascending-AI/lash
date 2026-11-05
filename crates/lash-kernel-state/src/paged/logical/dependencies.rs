use super::*;
use crate::arena::{Arena, ContentVersion};
use lash_kernel_doc::arena::ArenaHandle;

/// Immutable evidence used while evaluating a source interval. The global
/// ledger stamp covers all owner/member/departure facts, while arena stamps
/// cover the actual values and sizes read before their semantic publication.
pub struct Dependencies {
    ledger: Stamp,
    values: Vec<ContentVersion>,
}
impl Dependencies {
    /// The caller has authenticated canonical decoding without a source edit.
    /// Replace only that handle's old content version; retain every other fact.
    pub(super) fn decoded(&mut self, before: &ContentVersion, after: ContentVersion) {
        for value in &mut self.values {
            if value == before {
                *value = after.clone();
            }
        }
    }
    /// Combine facts captured at the same ledger identity and revision.
    pub fn extend(&mut self, next: Self) -> Result<(), HeapFault> {
        if !Rc::ptr_eq(&self.ledger.identity, &next.ledger.identity)
            || self.ledger.revision != next.ledger.revision
        {
            return Err(invalid("source dependencies span ledger revisions"));
        }
        for value in next.values {
            if !self.values.contains(&value) {
                self.values.push(value);
            }
        }
        Ok(())
    }
    pub fn capture(ledger: &Ledger, arena: &Arena, handles: &[ArenaHandle]) -> Option<Self> {
        let mut values = Vec::with_capacity(handles.len());
        for handle in handles {
            let version = arena.content_version(*handle)?;
            if !values.contains(&version) {
                values.push(version);
            }
        }
        Some(Self {
            ledger: ledger.stamp(),
            values,
        })
    }
    pub fn is_current(&self, ledger: &Ledger, arena: &Arena) -> bool {
        ledger.is_current(&self.ledger)
            && self.values.iter().all(|version| arena.content_is(version))
    }
    pub fn ledger(&self) -> &Stamp {
        &self.ledger
    }
}
