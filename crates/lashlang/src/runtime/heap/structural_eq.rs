//! Structural equality over heap graphs: two values are equal when the
//! objects they reach have the same kinds and members, cycles included.

use super::*;
use crate::runtime::deep_proportional_units;

impl Heap {
    #[cfg(test)]
    pub(crate) fn structural_eq(&self, left: &Value, right: &Value) -> Result<bool, RuntimeError> {
        self.structural_eq_with_work(left, right)
            .map(|(equal, _)| equal)
    }

    /// The equality, plus the proportional work the comparison performed in
    /// the units `charge_intrinsic_work` counts: one member per collection
    /// visited and one byte per text compared. The count depends only on the
    /// values, so the charge is the same on every replay.
    pub(crate) fn structural_eq_with_work(
        &self,
        left: &Value,
        right: &Value,
    ) -> Result<(bool, usize), RuntimeError> {
        let mut work = 0usize;
        let equal = self.structural_eq_inner(left, right, &mut BTreeSet::new(), &mut work)?;
        Ok((equal, work))
    }

    fn structural_eq_inner(
        &self,
        left: &Value,
        right: &Value,
        visited: &mut BTreeSet<(HeapId, HeapId)>,
        work: &mut usize,
    ) -> Result<bool, RuntimeError> {
        let (Value::Ref(left_id), Value::Ref(right_id)) = (left, right) else {
            // An inline pair compares member by member, descending through
            // every tuple, list, and record member it reaches — but only when
            // the kinds could match, since a mismatched pair fails in constant
            // work. A projected side materializes first, which descends too.
            let descends = matches!(
                (left, right),
                (Value::Tuple(_), Value::Tuple(_))
                    | (Value::List(_), Value::List(_))
                    | (Value::Record(_), Value::Record(_))
                    | (Value::String(_), Value::String(_))
            ) || matches!(left, Value::Projected(_))
                || matches!(right, Value::Projected(_));
            if descends {
                *work = work.saturating_add(
                    deep_proportional_units(left).saturating_add(deep_proportional_units(right)),
                );
            }
            return Ok(left == right);
        };
        if !visited.insert((*left_id, *right_id)) {
            return Ok(true);
        }
        match (self.get(*left_id)?, self.get(*right_id)?) {
            (HeapObject::Tuple(left), HeapObject::Tuple(right))
            | (HeapObject::List(left), HeapObject::List(right)) => {
                if left.len() != right.len() {
                    return Ok(false);
                }
                *work = work.saturating_add(left.len() + right.len());
                for (left, right) in left.iter().zip(right) {
                    if !self.structural_eq_inner(left, right, visited, work)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            (HeapObject::Record(left), HeapObject::Record(right)) => {
                if left.len() != right.len() {
                    return Ok(false);
                }
                *work = work.saturating_add(left.entries.len() + right.entries.len());
                for entry in &left.entries {
                    let Some(right_value) = right.get_symbol(entry.symbol) else {
                        return Ok(false);
                    };
                    if !self.structural_eq_inner(&entry.value, right_value, visited, work)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            (
                HeapObject::Closure {
                    function: left_function,
                    captures: left,
                    name: left_name,
                    length: left_length,
                },
                HeapObject::Closure {
                    function: right_function,
                    captures: right,
                    name: right_name,
                    length: right_length,
                },
            ) => {
                if left_function != right_function
                    || left.len() != right.len()
                    || left_name != right_name
                    || left_length != right_length
                {
                    return Ok(false);
                }
                *work = work.saturating_add(left.len() + right.len());
                for (left, right) in left.iter().zip(right) {
                    if !self.structural_eq_inner(left, right, visited, work)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            (HeapObject::BuiltinFunction(left), HeapObject::BuiltinFunction(right)) => {
                Ok(left == right)
            }
            (HeapObject::RegExp(left), HeapObject::RegExp(right)) => Ok(left == right),
            (HeapObject::Map(left), HeapObject::Map(right)) => {
                if left.entries.len() != right.entries.len() {
                    return Ok(false);
                }
                *work = work
                    .saturating_add(left.entries.len())
                    .saturating_add(right.entries.len());
                for ((left_key, left_value), (right_key, right_value)) in
                    left.entries.iter().zip(&right.entries)
                {
                    if !self.structural_eq_inner(left_key, right_key, visited, work)?
                        || !self.structural_eq_inner(left_value, right_value, visited, work)?
                    {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            (HeapObject::Set(left), HeapObject::Set(right)) => {
                if left.values.len() != right.values.len() {
                    return Ok(false);
                }
                *work = work
                    .saturating_add(left.values.len())
                    .saturating_add(right.values.len());
                for (left, right) in left.values.iter().zip(&right.values) {
                    if !self.structural_eq_inner(left, right, visited, work)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            (HeapObject::Date(left), HeapObject::Date(right)) => Ok(left == right),
            (HeapObject::Error(left), HeapObject::Error(right)) => {
                if left.kind != right.kind || left.message != right.message {
                    return Ok(false);
                }
                match (&left.cause, &right.cause) {
                    (Some(left), Some(right))
                        if !self.structural_eq_inner(left, right, visited, work)? =>
                    {
                        return Ok(false);
                    }
                    (None, None) | (Some(_), Some(_)) => {}
                    _ => return Ok(false),
                }
                match (&left.errors, &right.errors) {
                    (Some(left), Some(right)) => {
                        self.structural_eq_inner(left, right, visited, work)
                    }
                    (None, None) => Ok(true),
                    _ => Ok(false),
                }
            }
            (HeapObject::Url(left), HeapObject::Url(right)) => {
                *work = work.saturating_add(left.href.len().min(right.href.len()));
                Ok(left.href == right.href
                    && self.structural_eq_inner(
                        &left.search_params,
                        &right.search_params,
                        visited,
                        work,
                    )?)
            }
            (HeapObject::UrlSearchParams(left), HeapObject::UrlSearchParams(right)) => {
                *work = work
                    .saturating_add(left.entries.len())
                    .saturating_add(right.entries.len());
                Ok(left == right)
            }
            _ => Ok(false),
        }
    }
}
