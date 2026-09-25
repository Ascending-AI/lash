//! String.prototype methods whose semantics run through a RegExp —
//! `match`, `search` and `matchAll` take a receiver already resolved to a
//! heap RegExp by [`Vm::execute_javascript_regexp`].

use super::javascript::js_stdlib_error;
use super::javascript_regexp::{
    advance_string_index, bounded_utf16_input, collect_regress_match, push_utf16_range_bounded,
};
use super::*;

impl<H: ExecutionHost> Vm<'_, H> {
    /// The `regexp` argument of `String.prototype.match`/`search`. A RegExp is
    /// used as-is; anything else becomes `RegExp(pattern)` — `undefined` the
    /// empty pattern, every other value coerced through ToString — per the
    /// ECMA RegExpCreate the operation performs on a non-RegExp argument,
    /// which runs an object's own `toString`/`valueOf` (FIG-3652).
    pub(super) fn string_regexp_argument(
        &mut self,
        pattern: &Value,
    ) -> Result<HeapId, RuntimeError> {
        if let Value::Ref(id) = pattern
            && matches!(self.heap.get(*id)?, HeapObject::RegExp(_))
        {
            return Ok(*id);
        }
        let pattern = if matches!(pattern, Value::Undefined) {
            String::new()
        } else {
            self.heap.javascript_to_string(pattern)?
        };
        let Value::Ref(receiver) = self.construct_regexp(&[Value::String(pattern.into())])? else {
            unreachable!("RegExp allocation produces a heap reference")
        };
        Ok(receiver)
    }

    pub(super) fn string_match(
        &mut self,
        input: &str,
        receiver: HeapId,
    ) -> Result<Value, RuntimeError> {
        let global =
            matches!(self.heap.get(receiver)?, HeapObject::RegExp(re) if re.flags.contains('g'));
        if !global {
            return self.exec_regexp(receiver, input);
        }
        self.heap.set_regexp_last_index(receiver, 0)?;
        let units = bounded_utf16_input(&self.heap, input)?;
        let matches = self.regexp_matches(receiver, &units, 0, true, None)?;
        let mut values = Vec::new();
        let mut pending_bytes = 16_u64;
        for found in &matches {
            push_utf16_range_bounded(
                &self.heap,
                &mut values,
                &units,
                found.range.clone(),
                &mut pending_bytes,
            )?;
        }
        self.heap.set_regexp_last_index(receiver, 0)?;
        if values.is_empty() {
            Ok(Value::Null)
        } else {
            Ok(Value::List(values.into()))
        }
    }

    pub(super) fn string_search(
        &mut self,
        input: &str,
        receiver: HeapId,
    ) -> Result<i64, RuntimeError> {
        let units = bounded_utf16_input(&self.heap, input)?;
        let saved = self.heap.regexp_last_index(receiver)?.unwrap_or(0);
        let sticky =
            matches!(self.heap.get(receiver)?, HeapObject::RegExp(re) if re.flags.contains('y'));
        let found = self.first_regexp_match(receiver, &units, 0, sticky)?;
        self.heap.set_regexp_last_index(receiver, saved)?;
        Ok(found.map_or(-1, |found| found.range.start as i64))
    }

    pub(super) fn string_match_all(
        &mut self,
        input: &str,
        receiver: HeapId,
    ) -> Result<Value, RuntimeError> {
        let (global, unicode, sticky, start) = match self.heap.get(receiver)? {
            HeapObject::RegExp(regexp) => (
                regexp.flags.contains('g'),
                regexp.flags.contains('u'),
                regexp.flags.contains('y'),
                regexp.last_index as usize,
            ),
            _ => return Err(js_stdlib_error("matchAll requires a RegExp")),
        };
        if !global {
            return Err(self.regexp_type_error(
                "String.prototype.matchAll called with a non-global RegExp argument",
            ));
        }
        let units = bounded_utf16_input(&self.heap, input)?;
        let fuel = self.grant_regexp_fuel();
        let program = self.regexp_program(receiver)?;
        if unicode && sticky {
            self.collect_match_all_values(
                input,
                &units,
                program.try_find_from_utf16_anchored(&units, start, fuel),
                unicode,
                sticky,
                start,
            )
        } else if unicode {
            self.collect_match_all_values(
                input,
                &units,
                program.try_find_from_utf16(&units, start, fuel),
                unicode,
                sticky,
                start,
            )
        } else if sticky {
            self.collect_match_all_values(
                input,
                &units,
                program.try_find_from_ucs2_anchored(&units, start, fuel),
                unicode,
                sticky,
                start,
            )
        } else {
            self.collect_match_all_values(
                input,
                &units,
                program.try_find_from_ucs2(&units, start, fuel),
                unicode,
                sticky,
                start,
            )
        }
    }

    pub(super) fn collect_match_all_values<I>(
        &mut self,
        input: &str,
        units: &[u16],
        matches: I,
        unicode: bool,
        sticky: bool,
        start: usize,
    ) -> Result<Value, RuntimeError>
    where
        I: Iterator<Item = Result<lash_regress::Match, lash_regress::MatchError>>,
    {
        let mut values = Vec::new();
        let mut expected = start;
        for found in matches {
            let found = collect_regress_match(found)?;
            if sticky && found.range.start != expected {
                break;
            }
            self.heap.ensure_additional_logical_bytes(
                16_u64.saturating_add((values.len() as u64 + 1).saturating_mul(24)),
            )?;
            values
                .try_reserve_exact(1)
                .map_err(|_| RuntimeError::MemoryLimitExceeded {
                    limit: DEFAULT_HEAP_LOGICAL_BYTE_LIMIT,
                    attempted: u64::MAX,
                })?;
            expected = if found.range.is_empty() {
                advance_string_index(units, found.range.end, unicode)
            } else {
                found.range.end
            };
            values.push(self.allocate_match_result(input, units, &found)?);
        }
        Ok(Value::List(values.into()))
    }
}
