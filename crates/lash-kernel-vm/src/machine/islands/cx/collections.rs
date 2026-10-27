//! Completion measurement before publishing a staged flat Map write.

use super::{Cx, Measure, Plan, Prepared, Source, Value, immediate_measure};
use crate::data::immediate_size;

impl Cx<'_> {
    /// A short text is already normalized. The flat summary proves there
    /// are no descendant collections or aliases to walk after replacement.
    pub(super) fn map_completion(
        &self,
        plan: &Plan,
        args: &[Value],
        result: Option<&Value>,
    ) -> Option<u64> {
        let Prepared::Table {
            table,
            key: Value::Text(key),
            written,
        } = &self.held.as_ref()?.write
        else {
            return None;
        };
        let mut deep = self.arena.flat_charge_size(&Value::Map(*table)).ok()??;
        // Never subtract from a saturated projection of the exact summary.
        if deep == u64::MAX {
            return None;
        }
        let old = self.arena.table_get_text(*table, key).ok()?;
        let size = self
            .arena
            .len(*table)
            .ok()?
            .checked_add(u64::from(old.is_none()))?
            .checked_mul(2)?
            .checked_add(1)?;
        deep = match old {
            Some(old) => deep.checked_sub(immediate_size(&old)?)?,
            None => deep
                .checked_add(2)?
                .checked_add(immediate_size(&Value::Text(*key))?)?,
        }
        .checked_add(immediate_size(written)?)?;
        let mut failed = false;
        let units = plan.evaluate(|source, measure| {
            let value = match source {
                Source::Arg(index) => args.get(index),
                Source::Result => result,
                Source::Nothing => None,
            };
            let Some(value) = value else {
                return 0;
            };
            if matches!(value, Value::Map(object) if object == table) {
                return match measure {
                    Measure::Size => size,
                    Measure::DeepSize => deep,
                    Measure::NestedSize | Measure::Magnitude => 0,
                };
            }
            if let Some(units) = immediate_measure(value, measure) {
                return units;
            }
            // Another graph may reach the written map. Its completion
            // remains the ordinary body's after the write.
            failed = true;
            0
        });
        (!failed).then_some(units)
    }
}
