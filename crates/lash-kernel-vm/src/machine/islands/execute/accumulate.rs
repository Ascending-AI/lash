//! Binary local accumulation shares the statement's existing semantic owners.

use super::*;

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    #[inline(never)]
    pub(super) fn deferred(
        &mut self,
        frame: &mut Values<'_, 'e>,
        operation: &Deferred,
    ) -> Result<Next, Stop> {
        let (dst, constant, source, original) = match operation {
            Deferred::Source(pc) => return Err(self.at(*pc)),
            Deferred::Text {
                dst,
                constant,
                source,
                original,
            } => (*dst, *constant, *source, original),
        };
        let crate::compile::Constant::Deferred(literal @ lash_kernel_doc::Literal::Text(_)) =
            &frame.state.callee.stream.consts[constant.ix()]
        else {
            unreachable!("a text instruction names a text literal");
        };
        if !self.source_literal(frame, source, dst, literal, original)? {
            return Err(self.at(source.unpaid()));
        }
        if original.continues() {
            Ok(Next::Continue)
        } else {
            Err(self.at(source.pc() + 1))
        }
    }

    #[inline(always)]
    pub(super) fn accumulate(&mut self, frame: &mut Values<'_, 'e>, at: usize) {
        let left = match frame.slots[at] {
            SlotState::Value(Value::LongText(text)) => Some(text),
            _ => None,
        };
        let mut owned = false;
        frame.state.texts.retain(|(slot, text)| {
            if *slot != at {
                return true;
            }
            owned |= left == Some(*text);
            false
        });
        let owned = owned && left.is_some_and(|text| self.arena.extensible(text));
        self.accumulator = left.map(|left| crate::compile::Accumulator {
            left,
            owned,
            made: None,
        });
    }

    #[inline(always)]
    pub(super) fn operator_destination(
        &mut self,
        frame: &mut Values<'_, 'e>,
        site: &OperatorSite,
        values: &[Value],
        _covered: bool,
        to: &Destination,
    ) -> Result<Next, Stop> {
        if let Some(next) = self.prepared_destination(frame, site, values, to)? {
            return Ok(next);
        }
        let original = frame
            .state
            .source
            .as_ref()
            .is_some_and(|source| source.original.is_some());
        if !original
            && let Some(close) = &site.close
            && !self.close(close)
        {
            return Err(self.at_operands(close.head, site.reg, values));
        }
        Err(self.at_operands(site.pc, site.reg, values))
    }
}
