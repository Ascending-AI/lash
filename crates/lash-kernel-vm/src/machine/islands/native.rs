//! Canonical resident item and membership reads supply facts to source admission.

use super::*;

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    /// `list.get` / `tuple.get` on a valid resident position. This reads
    /// and measures only; the call admits its charge before committing.
    pub(super) fn item_answer(&self, site: &NativeSite, args: &[Value]) -> Option<(Value, u64)> {
        let Some(Direct::Item(kind)) = site.direct(self.exe) else {
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
        Some((value, units))
    }

    /// Resident reads supply the native formula to canonical source admission.
    fn read_formula(&self, site: &NativeSite, args: &[Value], value: Value) -> Option<u64> {
        measured_formula(
            self.arena,
            &self.exe.lib(site.lib).charge,
            args,
            Some(&value),
        )
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
        let Some(Direct::Member(table)) = site.direct(self.exe) else {
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
