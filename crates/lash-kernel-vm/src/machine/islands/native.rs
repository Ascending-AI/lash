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
        let length = match (site.direct, padded[0]) {
            (Some(Direct::Length(units)), Value::List(list)) => {
                self.arena.len(list).ok().map(|len| {
                    (
                        Value::Int(len as i64),
                        if self.charging { units } else { 0 },
                    )
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
                let (entry, fast) = function.prepared.as_ref()?;
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

    /// `list.get` / `tuple.get` on a valid resident position. This reads
    /// and measures only; the call admits its charge before committing.
    pub(super) fn item_answer(&self, site: &NativeSite, args: &[Value]) -> Option<(Value, u64)> {
        let Some(Direct::Item(kind)) = site.direct else {
            return None;
        };
        if self.tape.is_some() || self.projection().is_some() || args[0].kind() != kind {
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
            measured_formula(
                self.arena,
                &self.exe.lib(site.lib).charge,
                &args[..2],
                Some(&value),
            )?
        } else {
            0
        };
        #[cfg(test)]
        super::DIRECT_ITEMS.with(|n| n.set(n.get() + 1));
        Some((value, units))
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
        if self.projection().is_some() {
            return None;
        }
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
            measured_formula(
                self.arena,
                &self.exe.lib(site.lib).charge,
                &padded[..2],
                Some(&value),
            )?
        } else {
            0
        };
        Some((value, units))
    }
}
