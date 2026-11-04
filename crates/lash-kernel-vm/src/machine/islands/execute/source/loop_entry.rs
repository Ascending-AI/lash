//! Loop items, tests and occurrences share the actual source acceptance.
use super::*;
use crate::admission::SourceResources;
use lash_kernel_state::kernel_internals::logical::{PublicationValue, RootValue};
use room::publication::boundary::LoopEntry;

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_loop_entry(
        &mut self,
        frame: &mut Values<'_, '_>,
        entry: &LoopEntry,
        condition: Option<Value>,
        otherwise: usize,
    ) -> Result<bool, HeapFault> {
        if self.deferred_end.is_some() || self.spent() || self.projection().is_some() {
            return Ok(false);
        }
        let Some(Position::Loop {
            started,
            cursor,
            guard: None,
            ..
        }) = frame.state.control.last()
        else {
            return Ok(false);
        };
        let ordinal = *started;
        let (walked, next_cursor, value) = if let Some(cursor) = cursor {
            let position = match cursor {
                Cursor::List(_, at) | Cursor::Tuple(_, at) => *at as u64,
                _ => return Ok(false),
            };
            let Some(Ok((next, value))) = cursor.resident_step(self.arena) else {
                return Ok(false);
            };
            (
                Some((
                    cursor
                        .walked()
                        .ok_or_else(|| malformed("resident cursor has no collection"))?,
                    position,
                )),
                Some(next),
                value,
            )
        } else {
            (None, None, condition)
        };
        self.logical
            .borrow_mut()
            .transfer_source(self.arena, self.heap, &[], &[])?;
        let Some(activation) = frame.state.source.as_ref() else {
            return Ok(false);
        };
        if activation.original.is_some() {
            return Ok(false);
        }
        let Some(frontier) = activation.frontier.as_ref() else {
            return Ok(false);
        };
        let store = self.logical.clone();
        let ledger = store.borrow();
        let Some(bound) = entry.bind(
            frontier,
            &activation.invocation,
            &ledger,
            self.arena,
            self.heap,
            frame.slots,
            ordinal,
            walked,
            value,
            activation.loop_input.as_ref(),
            self.charged,
        )?
        else {
            return Ok(false);
        };
        let (inputs, outputs) = bound.roots();
        let outputs: Vec<_> = outputs
            .iter()
            .map(|(owner, value)| {
                (
                    *owner,
                    match value {
                        PublicationValue::Existing(from) => RootValue::Existing(*from),
                        PublicationValue::Native(value) => RootValue::Value(value),
                        _ => unreachable!("loop publication has only resident roots"),
                    },
                )
            })
            .collect();
        let registration = self
            .arena
            .registration()
            .map_err(publication::native_fault)?;
        let transaction =
            ledger.prepare_source_boundary(registration, self.heap, inputs, &outputs)?;
        let safe_point = self.charged;
        let Some(accepted) = bound.prepare_external(
            frontier,
            &ledger,
            self.heap,
            transaction,
            SourceResources {
                charged: &mut self.charged,
                counters: self.counters,
                bound: self.bound,
                slice: self.slice_end,
                depth: (self.base + u64::from(self.depth)).saturating_sub(1),
                limit: self.call_depth,
                safe_point,
            },
            self.memory_bound,
        )?
        else {
            return Ok(false);
        };
        drop(ledger);
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("loop activation disappeared"))?;
        let frontier = activation
            .frontier
            .as_mut()
            .ok_or_else(|| malformed("loop frontier disappeared"))?;
        let control = frame
            .state
            .control
            .last_mut()
            .ok_or_else(|| malformed("loop control disappeared"))?;
        let item_at = entry.item_at();
        let balance = accepted.publish_boundary(
            frontier,
            &mut store.borrow_mut(),
            |_, edits| {
                assert!(edits.is_empty());
                let Position::Loop {
                    started, cursor, ..
                } = control
                else {
                    unreachable!()
                };
                if let Some(next) = next_cursor {
                    *cursor = Some(next);
                    if let Some(value) = value {
                        frame.slots[item_at.unwrap_or_else(|| unreachable!())] =
                            SlotState::Value(value);
                        *started += 1;
                    } else {
                        frame.state.pc = otherwise;
                    }
                } else {
                    match value {
                        Some(Value::Bool(true)) => *started += 1,
                        Some(Value::Bool(false)) => frame.state.pc = otherwise,
                        _ => unreachable!(),
                    }
                }
                frame.state.statement = None;
            },
            self.recorder,
        )?;
        activation.loop_input = None;
        self.covered = false;
        self.logical_memory.set(balance.total()?);
        Ok(true)
    }
}
