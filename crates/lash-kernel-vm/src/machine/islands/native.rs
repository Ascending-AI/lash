//! Resident collection natives retain their native formula and slow exit.

use super::*;

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    /// The general call `site` of a native on `args`: its prepared entry's
    /// answer or its native's, charged its formula, or a stop at its op.
    #[inline(always)]
    pub(super) fn native(
        &mut self,
        site: &NativeSite,
        padded: &[Value; MAX_ARGS],
        given: usize,
    ) -> Answer {
        let args = &padded[..given];
        if let Some(close) = &site.close
            && !self.close(close)
        {
            return Err(self.at_operands(close.head, site.reg, args));
        }
        self.observed_native(site.lib, |island| island.native_answer(site, padded, given))
    }

    #[inline(always)]
    fn native_answer(
        &mut self,
        site: &NativeSite,
        padded: &[Value; MAX_ARGS],
        given: usize,
    ) -> Answer {
        let args = &padded[..given];
        if matches!(site.direct, Some(Direct::TextJoin))
            && let Some(answer) = self.joined_text(site, args)
        {
            return answer;
        }
        let length = match (site.direct, padded[0]) {
            (Some(Direct::Length(kind, units)), value)
                if value.kind() == kind && !self.arena.any_cold() =>
            {
                let handle = match value {
                    Value::List(handle)
                    | Value::Tuple(handle)
                    | Value::Map(handle)
                    | Value::Set(handle)
                    | Value::Record(handle) => Some(handle),
                    _ => None,
                };
                handle
                    .and_then(|handle| self.arena.len(handle).ok())
                    .and_then(|len| {
                        Some((
                            Value::Int(i64::try_from(len).ok()?),
                            if self.charging { units } else { 0 },
                        ))
                    })
            }
            _ => None,
        };
        if length.is_some()
            && self.charging
            && let Some(watching) = &self.projection()
        {
            watching.formula_enter();
            watching.formula_leave();
        }
        let charge = length
            .or_else(|| self.item_answer(site, padded))
            .or_else(|| self.membership(site, padded))
            .or_else(|| {
                let function = self.exe.lib(site.lib);
                let (entry, fast) = function.specialization.prepared.as_ref()?;
                self.depth_fits(site.depth).then_some(())?;
                let answer = self.answered(
                    function,
                    fast,
                    &padded[..function.arity],
                    entry.answer(&padded[..function.arity], self.charging)?,
                );
                if answer.is_some()
                    && let Some(watching) = &self.projection()
                {
                    watching.prepared();
                }
                answer
            });
        if let Some((value, units)) = charge {
            if !self.admit_native(units) {
                return Err(self.at_operands(site.pc, site.reg, args));
            }
            self.formula(units);
            return Ok(value);
        }
        self.native_heap(site, padded, given)
    }

    #[inline(never)]
    fn joined_text(&mut self, site: &NativeSite, args: &[Value]) -> Option<Answer> {
        if self.swift
            && self.resident_room(0)
            && let [Value::List(list), Value::Text(separator)] = args
        {
            let count = self.arena.len(*list).ok()?;
            let memory = self.memory;
            let free = self
                .ceiling
                .saturating_sub(self.register_bytes)
                .saturating_sub(self.arena.held())
                .saturating_sub(self.transient);
            if let Some((value, bytes)) = self
                .arena
                .join_text(*list, *separator, |needed| !memory || needed <= free)
            {
                if memory {
                    self.transient += bytes;
                }
                let units = if self.charging {
                    use crate::compile::Source;
                    use lash_kernel_doc::Measure;
                    let separator_bytes = separator.as_str().len() as u64;
                    let input_bytes = bytes - separator_bytes * count.saturating_sub(1);
                    self.exe.lib(site.lib).charge.evaluate(|source, measure| {
                        match (source, measure) {
                            (Source::Arg(0), Measure::Size) => count.saturating_add(1),
                            (Source::Arg(0), Measure::DeepSize) => count
                                .saturating_mul(2)
                                .saturating_add(input_bytes)
                                .saturating_add(1),
                            (Source::Arg(1), Measure::Size | Measure::DeepSize) => {
                                separator_bytes.saturating_add(1)
                            }
                            (Source::Result, Measure::Size | Measure::DeepSize) => {
                                bytes.saturating_add(1)
                            }
                            _ => 0,
                        }
                    })
                } else {
                    0
                };
                if !self.admit_native(units) {
                    return Some(Err(self.at_operands(site.pc, site.reg, args)));
                }
                self.formula(units);
                return Some(Ok(value));
            }
        }
        None
    }

    /// `list.get` / `tuple.get` on a valid resident position. This reads
    /// and measures only; the call admits its charge before committing.
    pub(super) fn item_answer(&self, site: &NativeSite, args: &[Value]) -> Option<(Value, u64)> {
        let Some(Direct::Item(kind)) = site.direct else {
            return None;
        };
        if self.tape.is_some() || args[0].kind() != kind {
            return None;
        }
        #[cfg(test)]
        if self.scratch.chilled {
            return None;
        }
        let handle = match args[0] {
            Value::List(h) | Value::Tuple(h) => h,
            _ => return None,
        };
        let value = self
            .arena
            .item(handle, crate::compile::position(&args[1])?)
            .ok()??;
        if self.arena.summary_of(&value).depth as usize > self.limits.value_depth() {
            return None;
        }
        let units = if self.charging {
            self.read_formula(site, &args[..2], value)?
        } else {
            0
        };
        #[cfg(test)]
        super::DIRECT_ITEMS.with(|n| n.set(n.get() + 1));
        Some((value, units))
    }

    /// Resident reads use the native's formula and observation even when
    /// they need no nursery or argument window. The caller commits those
    /// facts only after admitting the answer's charge.
    fn read_formula(&self, site: &NativeSite, args: &[Value], value: Value) -> Option<u64> {
        if let Some(watching) = self.projection() {
            watching.formula_enter();
        }
        let answer = measured_formula(
            self.arena,
            &self.exe.lib(site.lib).charge,
            args,
            Some(&value),
        );
        if let Some(watching) = self.projection() {
            watching.formula_leave();
        }
        answer
    }

    /// A membership test's answer ([`crate::compile::Lib::membership`]),
    /// looked up as the native's call looks the key up and charged its
    /// formula measured on the values, as its general call charges it;
    /// `None`, having committed nothing, where the lookup must leave to
    /// that call. A record's short key found absent keeps its proof
    /// across operations ([`cx::probe`]).
    #[inline(always)]
    pub(super) fn membership(
        &mut self,
        site: &NativeSite,
        padded: &[Value],
    ) -> Option<(Value, u64)> {
        let Some(Direct::Member(table)) = site.direct else {
            return None;
        };
        self.member_answer(site, table, padded)
    }

    /// [`Self::membership`] of `table`, out of line: most natives are none.
    #[inline(never)]
    fn member_answer(
        &mut self,
        site: &NativeSite,
        table: lash_kernel_doc::plan::Table,
        padded: &[Value],
    ) -> Option<(Value, u64)> {
        #[cfg(test)]
        if self.scratch.chilled {
            return None;
        }
        let found = cx::probe(
            self.arena,
            self.limits,
            &mut self.scratch.absent,
            table,
            padded[0],
            padded[1],
        )?;
        let value = Value::Bool(found.is_some());
        let units = if self.charging {
            self.read_formula(site, &padded[..2], value)?
        } else {
            0
        };
        Some((value, units))
    }
}
