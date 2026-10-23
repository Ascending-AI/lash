//! Bounded host reads: a prompt never copies an unbounded heap value.

use lash_kernel_state::paged::SemanticHeap;
use std::ops::ControlFlow;

use lash_kernel_doc::heap::{Element, EntityId, IntRef, PageReader, PageValue};

use super::{HeapFault, SessionEditor, SessionValue, metadata_key};

impl SessionEditor {
    /// Reads at most `budget` bytes of value payload and node allowance.
    /// Values beyond that budget are described without copying their payload.
    /// The limit applies to the whole reachable value, including field names.
    ///
    /// # Errors
    ///
    /// A fault in a page needed to inspect or copy the bounded value.
    pub fn preview(
        &mut self,
        name: &lash_vocabulary::names::Name,
        budget: usize,
    ) -> Result<Option<SessionValue>, HeapFault> {
        let Some(record) = self.bindings.get(&metadata_key(name), self.heap.pages())? else {
            return Ok(None);
        };
        let value = match record.value {
            super::BoundRecord::Value(value) => value,
            super::BoundRecord::Shared(id) => self.heap.variable_get(id)?,
        };
        let (kind, extent) = extent(&self.heap, &value)?;
        if fits(&self.heap, &value, &mut (budget as u64), &mut Vec::new(), 0)? {
            self.get(name)
        } else {
            Ok(Some(SessionValue::Unpreviewed { kind, extent }))
        }
    }
}

fn extent(heap: &impl PageReader, value: &PageValue) -> Result<(&'static str, u64), HeapFault> {
    Ok(match value {
        PageValue::Text(text) => ("text", text.summary.utf8_bytes),
        PageValue::Bytes(bytes) => ("bytes", bytes.len),
        PageValue::Int(IntRef::Paged { summary, .. }) => ("integer", summary.bits),
        PageValue::Tuple(tuple) => ("tuple", tuple.summary.len),
        PageValue::List(id) => ("list", heap.len(*id)?),
        PageValue::Map(id) => ("map", heap.len(*id)?),
        PageValue::Set(id) => ("set", heap.len(*id)?),
        PageValue::Record(id) => ("record", heap.len(*id)?),
        PageValue::Error(_) => ("error", 1),
        _ => ("value", 1),
    })
}

fn take(remaining: &mut u64, bytes: u64) -> bool {
    match remaining.checked_sub(bytes) {
        Some(left) => {
            *remaining = left;
            true
        }
        None => false,
    }
}

fn fits(
    heap: &impl PageReader,
    value: &PageValue,
    remaining: &mut u64,
    path: &mut Vec<EntityId>,
    depth: usize,
) -> Result<bool, HeapFault> {
    // Every copied node needs space even when its serialized value is tiny.
    // Depth stops here too, before an adversarial immutable tuple chain can
    // recurse past the host stack. The full reader still classifies cycles
    // and non-data values as opaque when they fit this inspection.
    if depth >= heap.value_depth_limit().min(64) || !take(remaining, 64) {
        return Ok(false);
    }
    Ok(match value {
        PageValue::Text(text) => take(remaining, text.summary.utf8_bytes),
        PageValue::Bytes(bytes) => take(remaining, bytes.len),
        PageValue::Int(IntRef::Paged { summary, .. }) => take(remaining, summary.bits.div_ceil(8)),
        PageValue::Int(IntRef::Inline(integer)) => take(remaining, integer.bits().div_ceil(8)),
        PageValue::Timestamp(timestamp) => {
            take(remaining, timestamp.nanoseconds.bits().div_ceil(8))
        }
        PageValue::Handle(handle) => take(remaining, (handle.kind.len() + handle.id.len()) as u64),
        PageValue::Error(error) => {
            take(remaining, error.kind.summary.utf8_bytes)
                && take(remaining, error.message.summary.utf8_bytes)
                && fits(heap, &heap.error_data(error)?, remaining, path, depth + 1)?
        }
        PageValue::Tuple(tuple) => {
            if tuple.summary.len > *remaining / 64 {
                return Ok(false);
            }
            let mut fits_all = Ok(true);
            heap.tuple_visit(tuple, &mut |member| {
                fits_all = fits(heap, member, remaining, path, depth + 1);
                if matches!(fits_all, Ok(true)) {
                    ControlFlow::Continue(())
                } else {
                    ControlFlow::Break(())
                }
            })?;
            fits_all?
        }
        PageValue::List(id) | PageValue::Map(id) | PageValue::Set(id) | PageValue::Record(id) => {
            if path.contains(id) {
                return Ok(true);
            }
            if heap.len(*id)? > *remaining / 64 {
                return Ok(false);
            }
            path.push(*id);
            let mut fits_all = Ok(true);
            heap.visit(*id, &mut |member| {
                fits_all = (|| match member {
                    Element::Item(value) => fits(heap, value, remaining, path, depth + 1),
                    Element::Entry { key, value } => {
                        Ok(fits(heap, key, remaining, path, depth + 1)?
                            && fits(heap, value, remaining, path, depth + 1)?)
                    }
                    Element::Field { name, value } => Ok(take(remaining, name.summary.utf8_bytes)
                        && fits(heap, value, remaining, path, depth + 1)?),
                })();
                if matches!(fits_all, Ok(true)) {
                    ControlFlow::Continue(())
                } else {
                    ControlFlow::Break(())
                }
            })?;
            path.pop();
            fits_all?
        }
        _ => true,
    })
}
