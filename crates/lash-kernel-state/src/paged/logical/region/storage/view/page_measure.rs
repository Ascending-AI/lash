//! Charge walks use canonical pages for cold bodies and the local overlay for edits.
use super::*;
use lash_kernel_doc::heap::{Element, IntRef, PageReader, PageValue};
use std::ops::ControlFlow;

impl StateLease<'_> {
    pub(super) fn is_cold_value(&self, value: LeaseValue) -> Result<bool, HeapFault> {
        Ok(match value.repr {
            Repr::Scalar(_) => false,
            Repr::Object(object, _) => self.local.slots[self.local.index(object)?]
                .existing
                .as_ref()
                .is_some_and(|existing| existing.cold),
        })
    }

    pub(super) fn measure_cold(
        &mut self,
        value: LeaseValue,
        selector: Measure,
        local: &mut Vec<LeaseValue>,
    ) -> Result<u64, LeaseFault> {
        let Repr::Object(object, Some(native)) = value.repr else {
            return Err(invalid("cold measure has no canonical value").into());
        };
        let slot = &self.local.slots[self.local.index(object)?];
        let page = if let Some(entity) = slot.existing().entity {
            match native {
                Value::List(_) => PageValue::List(entity),
                Value::Map(_) => PageValue::Map(entity),
                Value::Set(_) => PageValue::Set(entity),
                Value::Record(_) => PageValue::Record(entity),
                _ => return Ok(u64::from(!matches!(selector, Measure::NestedSize))),
            }
        } else {
            self.arena()
                .facts(
                    native
                        .handle()
                        .ok_or_else(|| invalid("cold value has no handle"))?,
                )
                .map_err(local_native)?
                .page
                .get()
                .cloned()
                .ok_or_else(|| invalid("cold immutable has no page form"))?
        };
        let mut pages = std::mem::take(&mut self.local.measure_pages);
        pages.clear();
        let result = (|| {
            capacity(&mut pages, 1)?;
            pages.push((page, matches!(selector, Measure::NestedSize)));
            let mut total = 0u64;
            while let Some((page, expand_only)) = pages.pop() {
                let mutable = matches!(
                    page,
                    PageValue::List(_)
                        | PageValue::Map(_)
                        | PageValue::Set(_)
                        | PageValue::Record(_)
                );
                if matches!(selector, Measure::NestedSize) && mutable {
                    if !expand_only {
                        total = total.saturating_add(1);
                    }
                    continue;
                }
                if mutable {
                    let entity = page
                        .object()
                        .ok_or_else(|| invalid("mutable page has no identity"))?;
                    let index = *self
                        .local
                        .entities
                        .get(&entity)
                        .ok_or_else(|| invalid("measured page is outside the entry graph"))?;
                    let slot = &mut self.local.slots[index as usize];
                    if slot.measured == self.local.measure_epoch {
                        continue;
                    }
                    // Resident edits supersede the immutable entry page through every alias.
                    if slot.existing().overlays.values().next().is_some() {
                        let native = slot
                            .existing()
                            .value
                            .ok_or_else(|| invalid("edited entry has no value"))?;
                        capacity(local, 1)?;
                        local.push(LeaseValue {
                            repr: Repr::Object(self.local.object(index), Some(native)),
                        });
                        continue;
                    }
                    slot.measured = self.local.measure_epoch;
                }
                if !expand_only {
                    total = total.saturating_add(match &page {
                        PageValue::Int(IntRef::Inline(integer)) => {
                            1u64.saturating_add(integer.bits().div_ceil(64))
                        }
                        PageValue::Int(IntRef::Paged { summary, .. }) => {
                            1u64.saturating_add(summary.bits.div_ceil(64))
                        }
                        PageValue::Text(text) => 1u64.saturating_add(text.summary.utf8_bytes),
                        PageValue::Bytes(bytes) => 1u64.saturating_add(bytes.len),
                        PageValue::Tuple(tuple) => 1u64.saturating_add(tuple.summary.len),
                        PageValue::Error(error) => 1u64
                            .saturating_add(error.kind.summary.utf8_bytes)
                            .saturating_add(error.message.summary.utf8_bytes),
                        PageValue::Map(entity) => {
                            1u64.saturating_add(self.heap.len(*entity)?.saturating_mul(2))
                        }
                        PageValue::List(entity)
                        | PageValue::Set(entity)
                        | PageValue::Record(entity) => 1u64.saturating_add(self.heap.len(*entity)?),
                        _ => 1,
                    });
                }
                match page {
                    PageValue::Tuple(tuple) => {
                        let mut failed = None;
                        self.heap.tuple_visit(&tuple, &mut |value| {
                            if let Err(error) = capacity(&mut pages, 1) {
                                failed = Some(error);
                                return ControlFlow::Break(());
                            }
                            pages.push((value.clone(), false));
                            ControlFlow::Continue(())
                        })?;
                        if let Some(error) = failed {
                            return Err(error.into());
                        }
                    }
                    PageValue::Error(error) => {
                        capacity(&mut pages, 1)?;
                        pages.push((self.heap.error_data(&error)?, false));
                    }
                    PageValue::List(entity)
                    | PageValue::Map(entity)
                    | PageValue::Set(entity)
                    | PageValue::Record(entity) => {
                        let mut failed = None;
                        self.heap.visit(entity, &mut |element| {
                            if let Err(error) = capacity(&mut pages, 2) {
                                failed = Some(error);
                                return ControlFlow::Break(());
                            }
                            match element {
                                Element::Item(value) | Element::Field { value, .. } => {
                                    pages.push((value.clone(), false))
                                }
                                Element::Entry { key, value } => {
                                    pages.push((key.clone(), false));
                                    pages.push((value.clone(), false));
                                }
                            }
                            ControlFlow::Continue(())
                        })?;
                        if let Some(error) = failed {
                            return Err(error.into());
                        }
                    }
                    _ => {}
                }
            }
            Ok(total)
        })();
        pages.clear();
        self.local.measure_pages = pages;
        result
    }
}
