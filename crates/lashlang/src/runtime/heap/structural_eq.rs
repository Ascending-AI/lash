//! Structural equality over heap graphs: two values are equal when the
//! objects they reach have the same kinds and members, cycles included.

use super::*;

impl Heap {
    pub(crate) fn structural_eq(&self, left: &Value, right: &Value) -> Result<bool, RuntimeError> {
        self.structural_eq_inner(left, right, &mut BTreeSet::new())
    }

    fn structural_eq_inner(
        &self,
        left: &Value,
        right: &Value,
        visited: &mut BTreeSet<(HeapId, HeapId)>,
    ) -> Result<bool, RuntimeError> {
        let (Value::Ref(left_id), Value::Ref(right_id)) = (left, right) else {
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
                for (left, right) in left.iter().zip(right) {
                    if !self.structural_eq_inner(left, right, visited)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            (HeapObject::Record(left), HeapObject::Record(right)) => {
                if left.len() != right.len() {
                    return Ok(false);
                }
                for entry in &left.entries {
                    let Some(right_value) = right.get_symbol(entry.symbol) else {
                        return Ok(false);
                    };
                    if !self.structural_eq_inner(&entry.value, right_value, visited)? {
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
                for (left, right) in left.iter().zip(right) {
                    if !self.structural_eq_inner(left, right, visited)? {
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
                for ((left_key, left_value), (right_key, right_value)) in
                    left.entries.iter().zip(&right.entries)
                {
                    if !self.structural_eq_inner(left_key, right_key, visited)?
                        || !self.structural_eq_inner(left_value, right_value, visited)?
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
                for (left, right) in left.values.iter().zip(&right.values) {
                    if !self.structural_eq_inner(left, right, visited)? {
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
                        if !self.structural_eq_inner(left, right, visited)? =>
                    {
                        return Ok(false);
                    }
                    (None, None) | (Some(_), Some(_)) => {}
                    _ => return Ok(false),
                }
                match (&left.errors, &right.errors) {
                    (Some(left), Some(right)) => self.structural_eq_inner(left, right, visited),
                    (None, None) => Ok(true),
                    _ => Ok(false),
                }
            }
            (HeapObject::Url(left), HeapObject::Url(right)) => Ok(left.href == right.href
                && self.structural_eq_inner(&left.search_params, &right.search_params, visited)?),
            (HeapObject::UrlSearchParams(left), HeapObject::UrlSearchParams(right)) => {
                Ok(left == right)
            }
            _ => Ok(false),
        }
    }
}
