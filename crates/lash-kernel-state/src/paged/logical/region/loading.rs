//! Entry decoding is demand driven and cannot change a source holding.
use super::*;

impl StateLease<'_> {
    pub fn bind_loader(&mut self, loader: Box<dyn LeaseLoader>) -> Result<(), HeapFault> {
        if self.local.frozen || self.loader.is_some() {
            return Err(invalid(
                "region projection loader is already bound or frozen",
            ));
        }
        self.loader = Some(loader);
        Ok(())
    }

    pub(super) fn load_head(&mut self, index: usize) -> Result<(), LeaseFault> {
        let slot = &self.local.slots[index];
        let Some(existing) = &slot.existing else {
            return Ok(());
        };
        if !existing.cold {
            return Ok(());
        }
        let handle = existing
            .value
            .and_then(|value| value.handle())
            .ok_or(LeaseFault::BoundaryRequired)?;
        let loader = self.loader.as_ref().ok_or(LeaseFault::BoundaryRequired)?;
        let ledger = self.heap.region_ledger()?;
        self.authenticate(&ledger)?;
        let before = self
            .arena()
            .content_version(handle)
            .ok_or_else(|| invalid("cold projection has no content version"))?;
        let fact = existing
            .entity
            .map(|_| ledger.authenticate_warm(self.arena(), handle, self.heap.pages()))
            .transpose()?;
        match &mut self.storage {
            LeaseStorage::Arena(authority) => loader.load(authority.write(), self.heap, handle)?,
            _ => return Err(invalid("region graph is already materialized").into()),
        }
        if let Some(fact) = fact {
            fact.finish(&ledger, self.arena())?;
        }
        if !self.arena().same_generation(&before) {
            return Err(invalid("canonical decoding changed its handle generation").into());
        }
        let after = self
            .arena()
            .content_version(handle)
            .ok_or_else(|| invalid("decoded projection has no content version"))?;
        self.dependencies.decoded(&before, after);
        drop(ledger);
        let cold = !self
            .arena()
            .region_body_resident(handle)
            .map_err(storage::local_native)?;
        if cold {
            return Err(LeaseFault::BoundaryRequired);
        }
        // The cold proxy carried a placeholder body; the decoded one is the
        // authenticated length every bounds check and measurement reads.
        let len = match self.arena().body(handle).map_err(storage::local_native)? {
            lash_kernel_doc::arena::Body::List(seq) | lash_kernel_doc::arena::Body::Tuple(seq) => {
                seq.len
            }
            lash_kernel_doc::arena::Body::Map(table)
            | lash_kernel_doc::arena::Body::Set(table)
            | lash_kernel_doc::arena::Body::Record(table) => table.len,
            _ => 0,
        };
        let next = if matches!(
            self.local.slots[index].kind,
            storage::Kind::Map | storage::Kind::Set | storage::Kind::Record
        ) {
            self.arena()
                .region_table_next(handle)
                .map_err(storage::local_native)?
        } else {
            self.local.slots[index].existing().next
        };
        let existing = self.local.slots[index].existing_mut();
        existing.cold = false;
        existing.len = len;
        existing.next = next;
        self.bind_callable(index as u32, handle)?;
        Ok(())
    }
}
