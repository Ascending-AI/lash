use super::super::{
    ErrorKind, ensure_javascript_string_size, javascript_string_size_error, javascript_to_number,
    javascript_to_string,
};
use super::javascript_json::{javascript_json_stringify, parse_javascript_json};
pub(super) use super::javascript_stdlib::*;
use super::*;
use std::collections::BTreeSet;

impl<H: ExecutionHost> Vm<'_, H> {
    pub(super) fn is_truthy_for_dialect(&self, value: &Value) -> Result<bool, RuntimeError> {
        if let Value::Ref(id) = value
            && self.reference_semantics
            && self.heap.is_javascript_vm_object(*id)?
        {
            return Ok(true);
        }
        Ok(is_truthy(value))
    }

    pub(super) fn read_dialect_field(
        &mut self,
        target: Value,
        field: &Name,
    ) -> Result<Value, RuntimeError> {
        if let Value::Ref(id) = target {
            if self.reference_semantics {
                return read_javascript_heap_field(&self.heap, id, field);
            }
            let target = self.heap.export_for_instruction(&Value::Ref(id))?;
            return read_field_direct(target, field);
        }
        if self.reference_semantics {
            read_javascript_field_direct(target, field)
        } else {
            read_field_direct(target, field)
        }
    }

    pub(super) fn read_dialect_index(
        &mut self,
        target: Value,
        index: Value,
    ) -> Result<Value, RuntimeError> {
        if let Value::Ref(id) = target {
            if self.reference_semantics {
                return read_javascript_heap_index(&self.heap, id, &index);
            }
            let target = self.heap.export_for_instruction(&Value::Ref(id))?;
            return read_index_direct(target, index);
        }
        if self.reference_semantics {
            let key = self.heap.javascript_to_string(&index)?;
            read_javascript_index_direct_with_key(target, &key)
        } else {
            read_index_direct(target, index)
        }
    }

    pub(super) async fn iterable_values_for_dialect(
        &mut self,
        iterable: Value,
    ) -> Result<ListValue, RuntimeError> {
        match iterable {
            Value::Ref(id) if self.reference_semantics => match self.heap.get(id)? {
                HeapObject::Map(map) => Ok(map
                    .entries
                    .iter()
                    .map(|(key, value)| Value::List(vec![key.clone(), value.clone()].into()))
                    .collect::<Vec<_>>()
                    .into()),
                HeapObject::Set(set) => Ok(set.values.clone().into()),
                HeapObject::UrlSearchParams(params) => Ok(params
                    .entries
                    .iter()
                    .map(|(name, value)| {
                        Value::List(
                            vec![Value::String(name.into()), Value::String(value.into())].into(),
                        )
                    })
                    .collect::<Vec<_>>()
                    .into()),
                HeapObject::RegExp(_) | HeapObject::Date(_) | HeapObject::Url(_) => {
                    Err(RuntimeError::ValidationFailed {
                        reason: format!(
                            "TS_FOR_OF_EXOTIC_UNSUPPORTED: {} is not iterable",
                            self.heap.get(id)?.kind_name()
                        ),
                    })
                }
                _ => {
                    let exported = self.heap.export_for_instruction(&Value::Ref(id))?;
                    iterable_values(exported).await
                }
            },
            iterable => iterable_values(iterable).await,
        }
    }

    pub(super) fn execute_javascript_unary(
        &mut self,
        op: JavaScriptUnaryOp,
    ) -> Result<(), RuntimeError> {
        let value = self.pop_stack()?;
        if op == JavaScriptUnaryOp::TypeOf
            && let Value::Ref(id) = value
        {
            let kind = self.heap.get(id)?.kind_name();
            self.stack.push(Value::String(
                if kind == "function" {
                    "function"
                } else {
                    "object"
                }
                .into(),
            ));
        } else if matches!(op, JavaScriptUnaryOp::Plus | JavaScriptUnaryOp::Negate) {
            let number = self.heap.javascript_to_number(&value)?;
            self.stack
                .push(Value::Number(if op == JavaScriptUnaryOp::Negate {
                    -number
                } else {
                    number
                }));
        } else if op == JavaScriptUnaryOp::Not && matches!(value, Value::Ref(_)) {
            self.stack.push(Value::Bool(false));
        } else {
            self.stack.push(eval_javascript_unary(value, op));
        }
        Ok(())
    }

    pub(super) fn execute_javascript_binary(
        &mut self,
        op: JavaScriptBinaryOp,
    ) -> Result<(), RuntimeError> {
        let mut right = self.pop_stack()?;
        let mut left = self.pop_stack()?;
        let strict = matches!(
            op,
            JavaScriptBinaryOp::StrictEqual | JavaScriptBinaryOp::StrictNotEqual
        );
        let loose = matches!(
            op,
            JavaScriptBinaryOp::LooseEqual | JavaScriptBinaryOp::LooseNotEqual
        );
        if op == JavaScriptBinaryOp::Add
            && [&left, &right].into_iter().any(
                |value| matches!(value, Value::Ref(id) if matches!(self.heap.get(*id), Ok(HeapObject::Date(_)))),
            )
        {
            return Err(js_stdlib_error(
                "TS_DATE_STRING_COERCION_PENDING: Date addition requires unavailable host-local string semantics; use .toISOString()",
            ));
        }
        if !strict {
            let both_objects = matches!(left, Value::Ref(_)) && matches!(right, Value::Ref(_));
            if !loose || !both_objects {
                if matches!(left, Value::Ref(_)) {
                    left = self.heap.javascript_to_primitive_string_or_number(&left)?;
                }
                if matches!(right, Value::Ref(_)) {
                    right = self.heap.javascript_to_primitive_string_or_number(&right)?;
                }
            }
        }
        if op == JavaScriptBinaryOp::Add {
            let left_primitive = self.heap.javascript_to_primitive_string_or_number(&left)?;
            let right_primitive = self.heap.javascript_to_primitive_string_or_number(&right)?;
            if matches!(left_primitive, Value::String(_))
                || matches!(right_primitive, Value::String(_))
            {
                let left = javascript_to_string(&left_primitive);
                let right = javascript_to_string(&right_primitive);
                let bytes = left
                    .len()
                    .checked_add(right.len())
                    .ok_or_else(|| javascript_string_size_error(usize::MAX))?;
                ensure_javascript_string_size(bytes)?;
                self.stack
                    .push(Value::String(format!("{left}{right}").into()));
                return Ok(());
            }
        }
        self.stack.push(eval_javascript_binary(left, op, right));
        Ok(())
    }

    pub(super) fn execute_javascript_split(&mut self) -> Result<(), RuntimeError> {
        let separator = self.pop_stack()?;
        let value = self.pop_stack()?;
        let separator = self.heap.export_for_instruction(&separator)?;
        let value = self.heap.export_for_instruction(&value)?;
        let values = javascript_split(&value, &separator)?;
        self.stack.push(self.heap.allocate_list(values)?);
        Ok(())
    }

    pub(super) fn execute_javascript_join(&mut self) -> Result<(), RuntimeError> {
        let separator = self.pop_stack()?;
        let value = self.pop_stack()?;
        let separator = self.heap.export_for_instruction(&separator)?;
        let value = self.heap.export_for_instruction(&value)?;
        self.stack
            .push(Value::String(javascript_join(&value, &separator)?.into()));
        Ok(())
    }

    pub(super) fn execute_javascript_stdlib(&mut self, argc: usize) -> Result<(), RuntimeError> {
        let mut values = Vec::with_capacity(argc);
        for _ in 0..argc {
            values.push(self.pop_stack()?);
        }
        values.reverse();
        if let [Value::String(method), value] = values.as_slice()
            && method.as_str() == "__jsonContainerKind"
        {
            let kind = match value {
                Value::Ref(id) => match self.heap.get(*id)? {
                    HeapObject::List(_) | HeapObject::Tuple(_) => "array",
                    HeapObject::Record(_) => "record",
                    _ => "opaque",
                },
                Value::List(_) | Value::Tuple(_) => "array",
                Value::Record(_) => "record",
                _ => "scalar",
            };
            self.stack.push(Value::String(kind.into()));
            return Ok(());
        }
        if let [Value::String(method), value] = values.as_slice()
            && method.as_str() == "__jsonHasOwnToJSON"
        {
            let has = match value {
                Value::Ref(id) => match self.heap.get(*id)? {
                    HeapObject::Record(record) => record.get("toJSON").is_some(),
                    _ => false,
                },
                Value::Record(record) => record.get("toJSON").is_some(),
                _ => false,
            };
            self.stack.push(Value::Bool(has));
            return Ok(());
        }
        if let [Value::String(method), Value::Ref(list)] = values.as_slice()
            && method.as_str() == "__singleCallbackResult"
        {
            let (HeapObject::List(values) | HeapObject::Tuple(values)) = self.heap.get(*list)?
            else {
                return Err(js_stdlib_error(
                    "callback result container must be an array",
                ));
            };
            self.stack
                .push(values.first().cloned().unwrap_or(Value::Undefined));
            return Ok(());
        }
        if let [Value::String(method), value] = values.as_slice()
            && method.as_str() == "__jsonHasCycle"
        {
            let has_cycle = javascript_json_has_cycle(
                &self.heap,
                value,
                &mut BTreeSet::new(),
                &mut BTreeSet::new(),
            )?;
            self.stack.push(Value::Bool(has_cycle));
            return Ok(());
        }
        if let [Value::String(method), Value::Ref(active), needle] = values.as_slice()
            && method.as_str() == "__jsonActiveContains"
        {
            let HeapObject::List(values) = self.heap.get(*active)? else {
                return Err(js_stdlib_error(
                    "JSON stringify active stack must be an array",
                ));
            };
            self.stack
                .push(Value::Bool(values.iter().any(|value| value == needle)));
            return Ok(());
        }
        if let [Value::String(method), value, rest @ ..] = values.as_slice()
            && method.as_str() == "JSON.stringify"
        {
            let result = javascript_substrate::javascript_json_stringify_with_options(
                &self.heap,
                value,
                rest.first(),
                rest.get(1),
            );
            match result {
                Ok(Some(json)) => self.stack.push(Value::String(json.into())),
                Ok(None) => self.stack.push(Value::Undefined),
                Err(RuntimeError::ValidationFailed { reason })
                    if reason.starts_with("TypeError: ") =>
                {
                    let error = self.heap.allocate_error(
                        ErrorKind::TypeError,
                        reason.trim_start_matches("TypeError: ").to_string(),
                        None,
                        None,
                    )?;
                    return Err(RuntimeError::UncaughtException { value: error });
                }
                Err(error) => return Err(error),
            }
            return Ok(());
        }
        if let [Value::String(method), Value::Ref(receiver)] = values.as_slice()
            && matches!(self.heap.get(*receiver)?, HeapObject::Error(_))
        {
            let result = match method.as_str() {
                "Object.keys" | "Object.values" | "Object.entries" => {
                    Some(Value::List(Vec::new().into()))
                }
                "JSON.stringify" => Some(Value::String("{}".into())),
                "Array.isArray" => Some(Value::Bool(false)),
                _ => None,
            };
            if let Some(result) = result {
                self.stack.push(result);
                return Ok(());
            }
        }
        if let [Value::String(method), Value::Ref(receiver)] = values.as_slice()
            && matches!(
                method.as_str(),
                "Object.keys" | "Object.values" | "Object.entries"
            )
        {
            let result = match self.heap.get(*receiver)? {
                HeapObject::Record(record) => {
                    let entries = ecma_record_entries(record);
                    match method.as_str() {
                        "Object.keys" => entries
                            .into_iter()
                            .map(|(key, _)| Value::String(key.into()))
                            .collect(),
                        "Object.values" => entries
                            .into_iter()
                            .map(|(_, value)| value.clone())
                            .collect(),
                        "Object.entries" => entries
                            .into_iter()
                            .map(|(key, value)| {
                                Value::List(vec![Value::String(key.into()), value.clone()].into())
                            })
                            .collect(),
                        _ => unreachable!(),
                    }
                }
                HeapObject::List(items) | HeapObject::Tuple(items) => match method.as_str() {
                    "Object.keys" => (0..items.len())
                        .map(|index| Value::String(index.to_string().into()))
                        .collect(),
                    "Object.values" => items.to_vec(),
                    "Object.entries" => items
                        .iter()
                        .enumerate()
                        .map(|(index, value)| {
                            Value::List(
                                vec![Value::String(index.to_string().into()), value.clone()].into(),
                            )
                        })
                        .collect(),
                    _ => unreachable!(),
                },
                _ => Vec::new(),
            };
            self.stack.push(Value::List(result.into()));
            return Ok(());
        }
        if let [Value::String(method), Value::Ref(receiver), key] = values.as_slice()
            && method.as_str() == "Object.hasOwn"
        {
            let key = self.heap.javascript_to_string(key)?;
            let has = match self.heap.get(*receiver)? {
                HeapObject::Record(record) => record.get(&key).is_some(),
                HeapObject::List(values) | HeapObject::Tuple(values) => {
                    key == "length"
                        || array_index_property(&key)
                            .is_some_and(|index| index < values.len() as u32)
                }
                _ => false,
            };
            self.stack.push(Value::Bool(has));
            return Ok(());
        }
        if let [Value::String(method), Value::Ref(receiver), args @ ..] = values.as_slice()
            && method.as_str() == "Object.assign"
            && matches!(self.heap.get(*receiver)?, HeapObject::Record(_))
        {
            let HeapObject::Record(target) = self.heap.get(*receiver)? else {
                unreachable!("record receiver checked")
            };
            let mut output = target.as_ref().clone();
            for source in args {
                if matches!(source, Value::Null | Value::Undefined) {
                    continue;
                }
                let source = match source {
                    Value::Ref(id) => match self.heap.get(*id)? {
                        HeapObject::Record(record) => Some(record.as_ref()),
                        _ => None,
                    },
                    Value::Record(record) => Some(record.as_ref()),
                    _ => None,
                };
                if let Some(source) = source {
                    for (key, value) in ecma_record_entries(source) {
                        output.insert(key.to_string(), value.clone());
                    }
                }
            }
            self.heap.replace_javascript_record(*receiver, output)?;
            self.stack.push(Value::Ref(*receiver));
            return Ok(());
        }
        if let [Value::String(method), Value::Ref(receiver), args @ ..] = values.as_slice()
            && !method.contains('.')
            && matches!(self.heap.get(*receiver)?, HeapObject::List(_))
            && self.execute_javascript_array_heap_method(method, *receiver, args)?
        {
            return Ok(());
        }
        if let [Value::String(method), Value::Ref(receiver), args @ ..] = values.as_slice()
            && !method.contains('.')
            && self.heap.is_javascript_exotic(*receiver)?
        {
            return self.execute_javascript_heap_method(method, *receiver, args);
        }
        if let [Value::String(method), Value::Ref(receiver)] = values.as_slice()
            && method.as_str() == "Lash.ArrayFromIterable"
        {
            let output = match self.heap.get(*receiver)? {
                HeapObject::UrlSearchParams(params) => Some(
                    params
                        .entries
                        .iter()
                        .map(|(name, value)| {
                            Value::List(
                                vec![Value::String(name.into()), Value::String(value.into())]
                                    .into(),
                            )
                        })
                        .collect::<Vec<_>>(),
                ),
                HeapObject::Map(map) => Some(
                    map.entries
                        .iter()
                        .map(|(key, value)| Value::List(vec![key.clone(), value.clone()].into()))
                        .collect(),
                ),
                HeapObject::Set(set) => Some(set.values.clone()),
                _ => None,
            };
            if let Some(output) = output {
                self.stack.push(Value::List(output.into()));
                return Ok(());
            }
        }
        if let [Value::String(method), args @ ..] = values.as_slice()
            && method.as_str() == "URL.canParse"
        {
            self.stack.push(self.execute_url_can_parse(args)?);
            return Ok(());
        }
        if let [Value::String(method), args @ ..] = values.as_slice()
            && let Some(result) = self.execute_javascript_date_static(method, args)?
        {
            self.stack.push(result);
            return Ok(());
        }
        if let [Value::String(method), source] = values.as_slice()
            && matches!(method.as_str(), "Array.from" | "Lash.ArrayFromIterable")
        {
            let record = match source {
                Value::Ref(id) => match self.heap.get(*id)? {
                    HeapObject::Record(record) => Some(record.as_ref()),
                    _ => None,
                },
                Value::Record(record) => Some(record.as_ref()),
                _ => None,
            };
            if let Some(record) = record {
                let length = record
                    .get("length")
                    .map(javascript_to_number)
                    .unwrap_or(0.0);
                let length = if length.is_nan() || length <= 0.0 {
                    0.0
                } else {
                    length.trunc()
                };
                if length > u32::MAX as f64 {
                    let error = self.heap.allocate_error(
                        ErrorKind::RangeError,
                        "Invalid array length".to_string(),
                        None,
                        None,
                    )?;
                    return Err(RuntimeError::UncaughtException { value: error });
                }
                self.heap.ensure_list_allocation_len(length as usize)?;
            }
        }
        for value in &mut values {
            if matches!(value, Value::Ref(_)) {
                *value = self.heap.export_for_instruction(value)?;
            }
        }
        let result = javascript_stdlib(&values)?;
        if let Value::String(value) = &result {
            ensure_javascript_string_size(value.len())?;
        }
        self.stack.push(result);
        Ok(())
    }

    fn execute_javascript_heap_method(
        &mut self,
        method: &str,
        receiver: HeapId,
        args: &[Value],
    ) -> Result<(), RuntimeError> {
        let kind = self.heap.get(receiver)?.kind_name();
        if matches!(kind, "URL" | "URLSearchParams") {
            if let Some(result) = self.execute_url_heap_method(kind, method, receiver, args)? {
                self.stack.push(result);
            }
            return Ok(());
        }
        if kind == "Date" {
            if let Some(result) = self.execute_javascript_date_method(method, receiver)? {
                self.stack.push(result);
            }
            return Ok(());
        }
        let result = match (kind, method, args) {
            ("RegExp", "valueOf", []) | ("Map", "valueOf", []) | ("Set", "valueOf", []) => {
                Some(Value::Ref(receiver))
            }
            ("RegExp", "toString", []) => {
                let HeapObject::RegExp(regexp) = self.heap.get(receiver)? else {
                    unreachable!("RegExp receiver kind was checked")
                };
                Some(Value::String(regexp_string(regexp).into()))
            }
            ("Map", "toString", []) => Some(Value::String("[object Map]".into())),
            ("Set", "toString", []) => Some(Value::String("[object Set]".into())),
            (kind, "toString", []) if ErrorKind::from_name(kind).is_some() => {
                let HeapObject::Error(error) = self.heap.get(receiver)? else {
                    unreachable!("Error receiver kind was checked")
                };
                Some(Value::String(
                    if error.message.is_empty() {
                        error.kind.name().to_string()
                    } else {
                        format!("{}: {}", error.kind.name(), error.message)
                    }
                    .into(),
                ))
            }
            (kind, "valueOf", []) if ErrorKind::from_name(kind).is_some() => {
                Some(Value::Ref(receiver))
            }
            ("Map", "get", [key]) => Some(
                self.heap
                    .map_get(receiver, key)?
                    .unwrap_or(Value::Undefined),
            ),
            ("Map", "has", [key]) => Some(Value::Bool(self.heap.map_has(receiver, key)?)),
            ("Map", "set", [key, value]) => {
                self.heap.map_set(receiver, key.clone(), value.clone())?;
                Some(Value::Ref(receiver))
            }
            ("Map", "delete", [key]) => Some(Value::Bool(self.heap.map_delete(receiver, key)?)),
            ("Map", "clear", []) => {
                self.heap.map_clear(receiver)?;
                Some(Value::Undefined)
            }
            ("Map", "keys", []) => Some(Value::List(
                self.heap
                    .map_entries(receiver)?
                    .expect("Map receiver was checked")
                    .into_iter()
                    .map(|(key, _)| key)
                    .collect::<Vec<_>>()
                    .into(),
            )),
            ("Map", "values", []) => Some(Value::List(
                self.heap
                    .map_entries(receiver)?
                    .expect("Map receiver was checked")
                    .into_iter()
                    .map(|(_, value)| value)
                    .collect::<Vec<_>>()
                    .into(),
            )),
            ("Map", "entries", []) => Some(Value::List(
                self.heap
                    .map_entries(receiver)?
                    .expect("Map receiver was checked")
                    .into_iter()
                    .map(|(key, value)| Value::List(vec![key, value].into()))
                    .collect::<Vec<_>>()
                    .into(),
            )),
            ("Map", "forEach", [function]) => {
                // Durable determinism deliberately snapshots the complete call
                // sequence here. Entries added during callbacks are not visited,
                // while an entry deleted before its turn remains in the snapshot.
                let calls = self
                    .heap
                    .map_entries(receiver)?
                    .expect("Map receiver was checked")
                    .into_iter()
                    .map(|(key, value)| vec![value, key, Value::Ref(receiver)])
                    .collect();
                self.begin_callback_driver(function.clone(), calls, false, true)?;
                None
            }
            ("Set", "has", [value]) => Some(Value::Bool(self.heap.set_has(receiver, value)?)),
            ("Set", "add", [value]) => {
                self.heap.set_add(receiver, value.clone())?;
                Some(Value::Ref(receiver))
            }
            ("Set", "delete", [value]) => Some(Value::Bool(self.heap.set_delete(receiver, value)?)),
            ("Set", "clear", []) => {
                self.heap.set_clear(receiver)?;
                Some(Value::Undefined)
            }
            ("Set", "keys" | "values" | "entries", []) => {
                let values = self
                    .heap
                    .set_values(receiver)?
                    .expect("Set receiver was checked");
                Some(Value::List(
                    if method == "entries" {
                        values
                            .into_iter()
                            .map(|value| Value::List(vec![value.clone(), value].into()))
                            .collect::<Vec<_>>()
                    } else {
                        values
                    }
                    .into(),
                ))
            }
            ("Set", "forEach", [function]) => {
                // As with Map, the durable callback driver consumes this cloned
                // snapshot: later additions are not visited and later deletions
                // do not remove an already-scheduled callback.
                let calls = self
                    .heap
                    .set_values(receiver)?
                    .expect("Set receiver was checked")
                    .into_iter()
                    .map(|value| vec![value.clone(), value, Value::Ref(receiver)])
                    .collect();
                self.begin_callback_driver(function.clone(), calls, false, true)?;
                None
            }
            (
                "Set",
                "union" | "intersection" | "difference" | "symmetricDifference",
                [Value::Ref(other)],
            ) if matches!(self.heap.get(*other)?, HeapObject::Set(_)) => {
                let left = self
                    .heap
                    .set_values(receiver)?
                    .expect("Set receiver was checked");
                let right = self
                    .heap
                    .set_values(*other)?
                    .expect("Set argument was checked");
                let mut output = Vec::new();
                match method {
                    "union" => {
                        output.extend(left.iter().cloned());
                        for value in &right {
                            if !self.heap.set_has(receiver, value)? {
                                output.push(value.clone());
                            }
                        }
                    }
                    "intersection" => {
                        for value in &left {
                            if self.heap.set_has(*other, value)? {
                                output.push(value.clone());
                            }
                        }
                    }
                    "difference" => {
                        for value in &left {
                            if !self.heap.set_has(*other, value)? {
                                output.push(value.clone());
                            }
                        }
                    }
                    "symmetricDifference" => {
                        for value in &left {
                            if !self.heap.set_has(*other, value)? {
                                output.push(value.clone());
                            }
                        }
                        for value in &right {
                            if !self.heap.set_has(receiver, value)? {
                                output.push(value.clone());
                            }
                        }
                    }
                    _ => unreachable!(),
                }
                Some(self.heap.allocate_set(output)?)
            }
            ("Set", "isSubsetOf" | "isSupersetOf" | "isDisjointFrom", [Value::Ref(other)])
                if matches!(self.heap.get(*other)?, HeapObject::Set(_)) =>
            {
                let left = self
                    .heap
                    .set_values(receiver)?
                    .expect("Set receiver was checked");
                let right = self
                    .heap
                    .set_values(*other)?
                    .expect("Set argument was checked");
                Some(Value::Bool(match method {
                    "isSubsetOf" => left
                        .iter()
                        .all(|value| self.heap.set_has(*other, value).unwrap_or(false)),
                    "isSupersetOf" => right
                        .iter()
                        .all(|value| self.heap.set_has(receiver, value).unwrap_or(false)),
                    "isDisjointFrom" => left
                        .iter()
                        .all(|value| !self.heap.set_has(*other, value).unwrap_or(false)),
                    _ => unreachable!(),
                }))
            }
            _ => {
                return Err(js_stdlib_error(format!(
                    "TS_METHOD_UNSUPPORTED: {kind}.{method} with {} argument(s)",
                    args.len()
                )));
            }
        };
        if let Some(result) = result {
            self.stack.push(result);
        }
        Ok(())
    }
}

fn javascript_json_has_cycle(
    heap: &Heap,
    value: &Value,
    active: &mut BTreeSet<HeapId>,
    visited: &mut BTreeSet<HeapId>,
) -> Result<bool, RuntimeError> {
    let Value::Ref(id) = value else {
        return Ok(false);
    };
    if active.contains(id) {
        return Ok(true);
    }
    if visited.contains(id) {
        return Ok(false);
    }
    let children: Vec<&Value> = match heap.get(*id)? {
        HeapObject::List(values) | HeapObject::Tuple(values) => values.iter().collect(),
        HeapObject::Record(record) => record.values().collect(),
        _ => return Ok(false),
    };
    active.insert(*id);
    for child in children {
        if javascript_json_has_cycle(heap, child, active, visited)? {
            return Ok(true);
        }
    }
    active.remove(id);
    visited.insert(*id);
    Ok(false)
}

fn javascript_stdlib(values: &[Value]) -> Result<Value, RuntimeError> {
    let Some(Value::String(method)) = values.first() else {
        return Err(js_stdlib_error("missing method discriminator"));
    };
    let args = &values[1..];
    if method.as_str() == "__reduceEmpty" {
        return Err(js_stdlib_error(
            "TypeError: Reduce of empty array with no initial value",
        ));
    }
    if method.contains('.') {
        return javascript_static_stdlib(method, args);
    }
    let Some((target, args)) = args.split_first() else {
        return Err(js_stdlib_error("missing receiver"));
    };
    match target {
        Value::String(value) => javascript_string_method(method, value, args),
        Value::List(items) | Value::Tuple(items) => {
            javascript_array_method(method, items.as_ref(), args)
        }
        Value::Number(value) => javascript_number_method(method, *value, args),
        Value::Null | Value::Undefined if matches!(method.as_str(), "toString" | "valueOf") => {
            Err(js_stdlib_error(format!(
                "TS_METHOD_UNSUPPORTED: cannot call `{method}` on null or undefined"
            )))
        }
        _ if method == "toString" && args.is_empty() => {
            Ok(Value::String(javascript_to_string(target).into()))
        }
        _ if method == "valueOf" && args.is_empty() => Ok(target.clone()),
        _ => Err(js_stdlib_error(format!(
            "TS_METHOD_UNSUPPORTED: method `{method}` is unavailable on this value"
        ))),
    }
}

fn javascript_static_stdlib(method: &str, args: &[Value]) -> Result<Value, RuntimeError> {
    use crate::runtime::javascript::{javascript_strict_equal, javascript_to_number};
    let args = normalized_static_arguments(method, args);
    match (method, args.as_slice()) {
        ("Object.keys", [Value::Record(record)]) => Ok(Value::List(
            ecma_record_entries(record)
                .into_iter()
                .map(|(key, _)| Value::String(key.into()))
                .collect::<Vec<_>>()
                .into(),
        )),
        ("Object.values", [Value::Record(record)]) => Ok(Value::List(
            ecma_record_entries(record)
                .into_iter()
                .map(|(_, value)| value.clone())
                .collect::<Vec<_>>()
                .into(),
        )),
        ("Object.entries", [Value::Record(record)]) => Ok(Value::List(
            ecma_record_entries(record)
                .into_iter()
                .map(|(key, value)| {
                    Value::List(vec![Value::String(key.into()), value.clone()].into())
                })
                .collect::<Vec<_>>()
                .into(),
        )),
        ("Object.keys", [Value::List(values) | Value::Tuple(values)]) => Ok(Value::List(
            (0..values.len())
                .map(|index| Value::String(index.to_string().into()))
                .collect::<Vec<_>>()
                .into(),
        )),
        ("Object.values", [Value::List(values) | Value::Tuple(values)]) => {
            Ok(Value::List(values.to_vec().into()))
        }
        ("Object.entries", [Value::List(values) | Value::Tuple(values)]) => Ok(Value::List(
            values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    Value::List(vec![Value::String(index.to_string().into()), value.clone()].into())
                })
                .collect::<Vec<_>>()
                .into(),
        )),
        ("Object.keys", [Value::String(value)]) => Ok(Value::List(
            value
                .encode_utf16()
                .enumerate()
                .map(|(index, _)| Value::String(index.to_string().into()))
                .collect::<Vec<_>>()
                .into(),
        )),
        ("Object.values", [Value::String(value)]) => Ok(Value::List(
            value
                .encode_utf16()
                .map(|unit| utf16_value(vec![unit]))
                .collect::<Result<Vec<_>, _>>()?
                .into(),
        )),
        ("Object.entries", [Value::String(value)]) => Ok(Value::List(
            value
                .encode_utf16()
                .enumerate()
                .map(|(index, unit)| {
                    Ok(Value::List(
                        vec![
                            Value::String(index.to_string().into()),
                            utf16_value(vec![unit])?,
                        ]
                        .into(),
                    ))
                })
                .collect::<Result<Vec<_>, RuntimeError>>()?
                .into(),
        )),
        (
            "Object.keys" | "Object.values" | "Object.entries",
            [Value::Bool(_) | Value::Number(_)],
        ) => Ok(Value::List(Vec::new().into())),
        ("Object.fromEntries", [Value::List(entries) | Value::Tuple(entries)]) => {
            let mut record = record_with_capacity(entries.len());
            for entry in entries.iter() {
                let (Value::List(pair) | Value::Tuple(pair)) = entry else {
                    return Err(js_stdlib_error("Object.fromEntries entry is not iterable"));
                };
                if pair.len() < 2 {
                    return Err(js_stdlib_error(
                        "Object.fromEntries entry has fewer than two values",
                    ));
                }
                record.insert(javascript_to_string(&pair[0]), pair[1].clone());
            }
            Ok(Value::Record(std::sync::Arc::new(record)))
        }
        ("Object.assign", [target, sources @ ..]) => {
            let Value::Record(target) = target else {
                return Err(js_stdlib_error(
                    "TypeError: Object.assign target must be a prototype-free object",
                ));
            };
            let mut output = target.as_ref().clone();
            for source in sources {
                if matches!(source, Value::Null | Value::Undefined) {
                    continue;
                }
                let Value::Record(source) = source else {
                    continue;
                };
                for (key, value) in ecma_record_entries(source) {
                    output.insert(key.to_string(), value.clone());
                }
            }
            Ok(Value::Record(std::sync::Arc::new(output)))
        }
        ("Object.hasOwn", [Value::Record(record), key]) => Ok(Value::Bool(
            record.get(&javascript_to_string(key)).is_some(),
        )),
        ("Object.hasOwn", [Value::List(values) | Value::Tuple(values), key]) => {
            let key = javascript_to_string(key);
            Ok(Value::Bool(
                key == "length"
                    || array_index_property(&key).is_some_and(|index| index < values.len() as u32),
            ))
        }
        ("Object.hasOwn", [Value::String(value), key]) => {
            let key = javascript_to_string(key);
            Ok(Value::Bool(
                key == "length"
                    || array_index_property(&key)
                        .is_some_and(|index| index < value.encode_utf16().count() as u32),
            ))
        }
        ("Object.hasOwn", [Value::Bool(_) | Value::Number(_), _]) => Ok(Value::Bool(false)),
        ("Object.is", [left, right]) => Ok(Value::Bool(match (left, right) {
            (Value::Number(left), Value::Number(right)) => {
                (left.is_nan() && right.is_nan()) || left.to_bits() == right.to_bits()
            }
            _ => javascript_strict_equal(left, right),
        })),
        ("Array.isArray", [value]) => Ok(Value::Bool(matches!(
            value,
            Value::List(_) | Value::Tuple(_)
        ))),
        ("Lash.ArrayFromIterable", [Value::List(values) | Value::Tuple(values)]) => {
            Ok(Value::List(values.to_vec().into()))
        }
        ("Lash.ArrayFromIterable", [Value::String(value)]) => Ok(Value::List(
            value
                .chars()
                .map(|character| Value::String(character.to_string().into()))
                .collect::<Vec<_>>()
                .into(),
        )),
        ("Lash.ArrayFromIterable", [Value::Record(record)]) => {
            let length = record
                .get("length")
                .map(javascript_to_number)
                .unwrap_or(0.0);
            let length = if length.is_nan() || length <= 0.0 {
                0
            } else {
                length.trunc().min(u32::MAX as f64) as usize
            };
            Ok(Value::List(
                (0..length)
                    .map(|index| {
                        record
                            .get(&index.to_string())
                            .cloned()
                            .unwrap_or(Value::Undefined)
                    })
                    .collect::<Vec<_>>()
                    .into(),
            ))
        }
        ("Array.from", [Value::List(values) | Value::Tuple(values)]) => {
            Ok(Value::List(values.to_vec().into()))
        }
        ("Array.from", [Value::String(value)]) => Ok(Value::List(
            value
                .chars()
                .map(|character| Value::String(character.to_string().into()))
                .collect::<Vec<_>>()
                .into(),
        )),
        ("Array.from", [Value::Record(record)]) => {
            let length = record
                .get("length")
                .map(javascript_to_number)
                .unwrap_or(0.0);
            let length = if length.is_nan() || length <= 0.0 {
                0
            } else {
                length.trunc().min(u32::MAX as f64) as usize
            };
            Ok(Value::List(
                (0..length)
                    .map(|index| {
                        record
                            .get(&index.to_string())
                            .cloned()
                            .unwrap_or(Value::Undefined)
                    })
                    .collect::<Vec<_>>()
                    .into(),
            ))
        }
        ("Array.of", values) => Ok(Value::List(values.to_vec().into())),
        ("String.fromCharCode", values) => utf16_value(
            values
                .iter()
                .map(|value| to_uint16(javascript_to_number(value)))
                .collect(),
        ),
        ("String.fromCodePoint", values) => {
            let mut output = String::new();
            for value in values {
                let point = javascript_to_number(value);
                if !point.is_finite()
                    || point.fract() != 0.0
                    || !(0.0..=0x10ffff as f64).contains(&point)
                    || (0xd800 as f64..=0xdfff as f64).contains(&point)
                {
                    return Err(js_stdlib_error(
                        "String.fromCodePoint received an invalid code point",
                    ));
                }
                output.push(char::from_u32(point as u32).expect("validated code point"));
            }
            Ok(Value::String(output.into()))
        }
        ("Number.isFinite", [Value::Number(value)]) => Ok(Value::Bool(value.is_finite())),
        ("Number.isFinite", [_]) => Ok(Value::Bool(false)),
        ("Number.isInteger", [Value::Number(value)]) => {
            Ok(Value::Bool(value.is_finite() && value.fract() == 0.0))
        }
        ("Number.isInteger", [_]) => Ok(Value::Bool(false)),
        ("Number.isNaN", [Value::Number(value)]) => Ok(Value::Bool(value.is_nan())),
        ("Number.isNaN", [_]) => Ok(Value::Bool(false)),
        ("Number.isSafeInteger", [Value::Number(value)]) => Ok(Value::Bool(
            value.is_finite() && value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0,
        )),
        ("Number.isSafeInteger", [_]) => Ok(Value::Bool(false)),
        ("Number.parseFloat", [value]) => Ok(Value::Number(parse_float_prefix(
            &javascript_to_string(value),
        ))),
        ("Number.parseInt", [value]) => Ok(Value::Number(parse_int_prefix(
            &javascript_to_string(value),
            None,
        ))),
        ("Number.parseInt", [value, radix]) => Ok(Value::Number(parse_int_prefix(
            &javascript_to_string(value),
            Some(javascript_to_number(radix)),
        ))),
        ("JSON.parse", [Value::String(value)]) => parse_javascript_json(value),
        ("JSON.stringify", [Value::Undefined]) => Ok(Value::Undefined),
        ("JSON.stringify", [value]) => {
            javascript_json_stringify(value).map(|value| Value::String(value.into()))
        }
        ("Math.abs", [value]) => Ok(Value::Number(javascript_to_number(value).abs())),
        ("Math.acos", [value]) => Ok(Value::Number(javascript_to_number(value).acos())),
        ("Math.asin", [value]) => Ok(Value::Number(javascript_to_number(value).asin())),
        ("Math.acosh", [value]) => Ok(Value::Number(javascript_to_number(value).acosh())),
        ("Math.asinh", [value]) => Ok(Value::Number(javascript_to_number(value).asinh())),
        ("Math.atan", [value]) => Ok(Value::Number(javascript_to_number(value).atan())),
        ("Math.atan2", [y, x]) => Ok(Value::Number(
            javascript_to_number(y).atan2(javascript_to_number(x)),
        )),
        ("Math.atanh", [value]) => Ok(Value::Number(javascript_to_number(value).atanh())),
        ("Math.cbrt", [value]) => Ok(Value::Number(javascript_to_number(value).cbrt())),
        ("Math.ceil", [value]) => Ok(Value::Number(javascript_to_number(value).ceil())),
        ("Math.clz32", [value]) => Ok(Value::Number(
            to_uint32(javascript_to_number(value)).leading_zeros() as f64,
        )),
        ("Math.cos", [value]) => Ok(Value::Number(javascript_to_number(value).cos())),
        ("Math.cosh", [value]) => Ok(Value::Number(javascript_to_number(value).cosh())),
        ("Math.exp", [value]) => Ok(Value::Number(javascript_to_number(value).exp())),
        ("Math.expm1", [value]) => Ok(Value::Number(javascript_to_number(value).exp_m1())),
        ("Math.floor", [value]) => Ok(Value::Number(javascript_to_number(value).floor())),
        ("Math.fround", [value]) => Ok(Value::Number(javascript_to_number(value) as f32 as f64)),
        ("Math.hypot", values) => Ok(Value::Number(javascript_hypot(values))),
        ("Math.imul", [left, right]) => Ok(Value::Number(
            (to_uint32(javascript_to_number(left))
                .wrapping_mul(to_uint32(javascript_to_number(right))) as i32) as f64,
        )),
        ("Math.log", [value]) => Ok(Value::Number(javascript_to_number(value).ln())),
        ("Math.log1p", [value]) => Ok(Value::Number(javascript_to_number(value).ln_1p())),
        ("Math.log10", [value]) => Ok(Value::Number(javascript_to_number(value).log10())),
        ("Math.log2", [value]) => Ok(Value::Number(javascript_to_number(value).log2())),
        ("Math.round", [value]) => Ok(Value::Number(javascript_round(javascript_to_number(value)))),
        ("Math.trunc", [value]) => Ok(Value::Number(javascript_to_number(value).trunc())),
        ("Math.max", values) => Ok(Value::Number(javascript_extreme(values, true))),
        ("Math.min", values) => Ok(Value::Number(javascript_extreme(values, false))),
        ("Math.pow", [base, exponent]) => Ok(Value::Number(javascript_pow(
            javascript_to_number(base),
            javascript_to_number(exponent),
        ))),
        ("Math.sqrt", [value]) => Ok(Value::Number(javascript_to_number(value).sqrt())),
        ("Math.sin", [value]) => Ok(Value::Number(javascript_to_number(value).sin())),
        ("Math.sinh", [value]) => Ok(Value::Number(javascript_to_number(value).sinh())),
        ("Math.tan", [value]) => Ok(Value::Number(javascript_to_number(value).tan())),
        ("Math.tanh", [value]) => Ok(Value::Number(javascript_to_number(value).tanh())),
        ("Math.sign", [value]) => {
            let value = javascript_to_number(value);
            Ok(Value::Number(if value.is_nan() || value == 0.0 {
                value
            } else {
                value.signum()
            }))
        }
        _ => Err(js_stdlib_error(format!(
            "TS_METHOD_UNSUPPORTED: unsupported call `{method}` with {} argument(s)",
            args.len()
        ))),
    }
}

fn javascript_string_method(
    method: &str,
    value: &str,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    use crate::runtime::javascript::javascript_to_number;
    let args = normalized_instance_arguments(method, args);
    let units = value.encode_utf16().collect::<Vec<_>>();
    match (method, args.as_slice()) {
        ("at", [index]) => {
            let index = relative_index(javascript_to_number(index), units.len());
            index.map_or(Ok(Value::Undefined), |index| {
                utf16_value(vec![units[index]])
            })
        }
        ("charAt", []) => units
            .first()
            .copied()
            .map_or(Ok(Value::String("".into())), |unit| utf16_value(vec![unit])),
        ("charAt", [index]) => relative_nonnegative_index(javascript_to_number(index), units.len())
            .map_or(Ok(Value::String("".into())), |index| {
                utf16_value(vec![units[index]])
            }),
        ("charCodeAt", []) => Ok(Value::Number(
            units.first().map_or(f64::NAN, |value| *value as f64),
        )),
        ("charCodeAt", [index]) => Ok(Value::Number(
            relative_nonnegative_index(javascript_to_number(index), units.len())
                .map_or(f64::NAN, |index| units[index] as f64),
        )),
        ("codePointAt", []) => code_point_at(&units, 0),
        ("codePointAt", [index]) => {
            relative_nonnegative_index(javascript_to_number(index), units.len())
                .map_or(Ok(Value::Undefined), |index| code_point_at(&units, index))
        }
        ("concat", values) => {
            let values = values.iter().map(javascript_to_string).collect::<Vec<_>>();
            let bytes = values
                .iter()
                .try_fold(value.len(), |total, item| total.checked_add(item.len()));
            ensure_javascript_string_size(
                bytes.ok_or_else(|| javascript_string_size_error(usize::MAX))?,
            )?;
            let mut output = value.to_string();
            for item in values {
                output.push_str(&item);
            }
            Ok(Value::String(output.into()))
        }
        ("startsWith", [needle]) => string_starts_with(&units, needle, 0),
        ("startsWith", [needle, position]) => string_starts_with(
            &units,
            needle,
            clamp_nonnegative_index(javascript_to_number(position), units.len()),
        ),
        ("endsWith", [needle]) | ("endsWith", [needle, Value::Undefined]) => {
            string_ends_with(&units, needle, units.len())
        }
        ("endsWith", [needle, position]) => string_ends_with(
            &units,
            needle,
            clamp_nonnegative_index(javascript_to_number(position), units.len()),
        ),
        ("includes", [needle]) => string_includes(&units, needle, 0),
        ("includes", [needle, position]) => string_includes(
            &units,
            needle,
            clamp_nonnegative_index(javascript_to_number(position), units.len()),
        ),
        ("indexOf", [needle]) => string_index_of(&units, needle, 0),
        ("indexOf", [needle, position]) => string_index_of(
            &units,
            needle,
            clamp_nonnegative_index(javascript_to_number(position), units.len()),
        ),
        ("lastIndexOf", [needle]) => string_last_index_of(&units, needle, units.len()),
        ("lastIndexOf", [needle, Value::Undefined]) => {
            string_last_index_of(&units, needle, units.len())
        }
        ("lastIndexOf", [needle, position]) => {
            let position = javascript_to_number(position);
            string_last_index_of(
                &units,
                needle,
                if position.is_nan() {
                    units.len()
                } else {
                    clamp_nonnegative_index(position, units.len())
                },
            )
        }
        ("padStart", [length]) | ("padStart", [length, Value::Undefined]) => {
            pad_string(value, javascript_to_number(length), " ", true)
        }
        ("padStart", [length, fill]) => pad_string(
            value,
            javascript_to_number(length),
            &javascript_to_string(fill),
            true,
        ),
        ("padEnd", [length]) | ("padEnd", [length, Value::Undefined]) => {
            pad_string(value, javascript_to_number(length), " ", false)
        }
        ("padEnd", [length, fill]) => pad_string(
            value,
            javascript_to_number(length),
            &javascript_to_string(fill),
            false,
        ),
        ("repeat", [count]) => {
            let count = javascript_to_number(count);
            let count = if count.is_nan() { 0.0 } else { count };
            if !count.is_finite() || count < 0.0 {
                return Err(js_stdlib_error("String.repeat count is out of range"));
            }
            let count = count.trunc() as usize;
            let output_bytes = value
                .len()
                .checked_mul(count)
                .ok_or_else(|| javascript_string_size_error(usize::MAX))?;
            ensure_javascript_string_size(output_bytes)?;
            Ok(Value::String(value.repeat(count).into()))
        }
        ("replace", [needle, replacement]) => replace_string(
            value,
            &javascript_to_string(needle),
            &javascript_to_string(replacement),
        )
        .map(|value| Value::String(value.into())),
        ("replaceAll", [needle, replacement]) => replace_all_string(
            value,
            &javascript_to_string(needle),
            &javascript_to_string(replacement),
        )
        .map(|value| Value::String(value.into())),
        ("slice", bounds) => slice_utf16(&units, bounds, true),
        ("substring", bounds) => substring_utf16(&units, bounds),
        ("split", []) => Ok(Value::List(vec![Value::String(value.into())].into())),
        ("split", [separator]) => Ok(Value::List(
            javascript_split(&Value::String(value.into()), separator)?.into(),
        )),
        ("split", [separator, limit]) => {
            let mut values = javascript_split(&Value::String(value.into()), separator)?;
            let limit = javascript_to_number(limit);
            let limit = if limit.is_nan() || limit <= 0.0 {
                0
            } else {
                (limit.trunc() as usize).min(u32::MAX as usize)
            };
            values.truncate(limit);
            Ok(Value::List(values.into()))
        }
        ("toLowerCase", []) => Ok(Value::String(value.to_lowercase().into())),
        ("toUpperCase", []) => Ok(Value::String(value.to_uppercase().into())),
        ("trim", []) => Ok(Value::String(
            value
                .trim_matches(super::super::javascript::is_ecma_string_whitespace)
                .into(),
        )),
        ("trimStart", []) => Ok(Value::String(
            value
                .trim_start_matches(super::super::javascript::is_ecma_string_whitespace)
                .into(),
        )),
        ("trimEnd", []) => Ok(Value::String(
            value
                .trim_end_matches(super::super::javascript::is_ecma_string_whitespace)
                .into(),
        )),
        ("toString", []) | ("valueOf", []) => Ok(Value::String(value.into())),
        _ => Err(js_stdlib_error(format!(
            "TS_METHOD_UNSUPPORTED: String.{method}"
        ))),
    }
}

fn javascript_array_method(
    method: &str,
    items: &[Value],
    args: &[Value],
) -> Result<Value, RuntimeError> {
    use crate::runtime::javascript::javascript_to_number;
    let argument_count = args.len();
    let args = normalized_instance_arguments(method, args);
    match (method, args.as_slice()) {
        ("__singleCallbackResult", []) => Ok(items.first().cloned().unwrap_or(Value::Undefined)),
        ("__appendFlatMap", [value]) => {
            let mut output = items.to_vec();
            match value {
                Value::List(values) | Value::Tuple(values) => output.extend(values.iter().cloned()),
                value => output.push(value.clone()),
            }
            Ok(Value::List(output.into()))
        }
        ("at", [index]) => Ok(relative_index(javascript_to_number(index), items.len())
            .map_or(Value::Undefined, |index| items[index].clone())),
        ("concat", values) => {
            let mut output = items.to_vec();
            for value in values {
                match value {
                    Value::List(values) | Value::Tuple(values) => {
                        output.extend(values.iter().cloned())
                    }
                    value => output.push(value.clone()),
                }
            }
            Ok(Value::List(output.into()))
        }
        ("includes", [needle]) => array_includes(items, needle, 0),
        ("includes", [needle, from]) => array_includes(
            items,
            needle,
            clamp_relative_index(javascript_to_number(from), items.len()),
        ),
        ("indexOf", [needle]) => array_index_of(items, needle, 0),
        ("indexOf", [needle, from]) => array_index_of(
            items,
            needle,
            clamp_relative_index(javascript_to_number(from), items.len()),
        ),
        ("lastIndexOf", [needle]) => array_last_index_of(items, needle, items.len()),
        ("lastIndexOf", [needle, Value::Undefined]) if argument_count < 2 => {
            array_last_index_of(items, needle, items.len())
        }
        ("lastIndexOf", [needle, from]) => {
            last_index_exclusive(javascript_to_number(from), items.len())
                .map_or(Ok(Value::Number(-1.0)), |end| {
                    array_last_index_of(items, needle, end)
                })
        }
        ("join", []) => Ok(Value::String(
            javascript_join(&Value::List(items.to_vec().into()), &Value::Undefined)?.into(),
        )),
        ("join", [separator]) => Ok(Value::String(
            javascript_join(&Value::List(items.to_vec().into()), separator)?.into(),
        )),
        ("flat", depth) => {
            let depth = depth.first().map_or(1, |value| {
                let depth = javascript_to_number(value);
                if depth.is_nan() || depth <= 0.0 {
                    0
                } else if depth == f64::INFINITY {
                    usize::MAX
                } else {
                    depth.trunc() as usize
                }
            });
            let mut output = Vec::new();
            flatten_array(items, depth, &mut output);
            Ok(Value::List(output.into()))
        }
        ("slice", bounds) => {
            let start = match bounds.first() {
                None | Some(Value::Undefined) => 0.0,
                Some(value) => javascript_to_number(value),
            };
            // Absent and explicitly `undefined` are the same thing here.
            let end = match bounds.get(1) {
                None | Some(Value::Undefined) => items.len() as f64,
                Some(value) => javascript_to_number(value),
            };
            let start = clamp_relative_index(start, items.len());
            let end = clamp_relative_index(end, items.len()).max(start);
            Ok(Value::List(items[start..end].to_vec().into()))
        }
        ("toString", []) => Ok(Value::String(
            javascript_join(&Value::List(items.to_vec().into()), &Value::Undefined)?.into(),
        )),
        // Pair each item with its index so a two-parameter `map` callback can
        // be driven by the VM's one-argument map. Not a guest-visible method:
        // the lowerer emits it and nothing parses it.
        ("__enumerate", []) => Ok(Value::List(
            items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    Value::List(vec![item.clone(), Value::Number(index as f64)].into())
                })
                .collect::<Vec<_>>()
                .into(),
        )),
        _ => Err(js_stdlib_error(format!(
            "TS_METHOD_UNSUPPORTED: Array.{method}"
        ))),
    }
}

fn javascript_number_method(
    method: &str,
    value: f64,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    let digits = |default: i64, min: i64| -> Result<i64, RuntimeError> {
        let value = args
            .first()
            .map(javascript_to_number)
            .unwrap_or(default as f64);
        let value = if value.is_nan() {
            0
        } else {
            value.trunc() as i64
        };
        if !(min..=100).contains(&value) {
            return Err(js_stdlib_error(
                "RangeError: precision must be between 0 and 100",
            ));
        }
        Ok(value)
    };
    let rendered = match method {
        "toFixed" => {
            let digits = digits(0, 0)? as u8;
            ryu_js::Buffer::new()
                .format_to_fixed(value, digits)
                .to_string()
        }
        "toExponential" => {
            let fraction = if args.is_empty() || matches!(args, [Value::Undefined]) {
                None
            } else {
                Some(digits(0, 0)? as usize)
            };
            javascript_exponential(value, fraction)
        }
        "toPrecision" if args.is_empty() || matches!(args, [Value::Undefined]) => {
            javascript_to_string(&Value::Number(value))
        }
        "toPrecision" => javascript_precision(value, digits(1, 1)? as usize),
        "toString" if args.is_empty() => javascript_to_string(&Value::Number(value)),
        "valueOf" if args.is_empty() => return Ok(Value::Number(value)),
        _ => {
            return Err(js_stdlib_error(format!(
                "TS_METHOD_UNSUPPORTED: Number.{method}"
            )));
        }
    };
    Ok(Value::String(rendered.into()))
}

fn javascript_exponential(value: f64, fraction: Option<usize>) -> String {
    if !value.is_finite() {
        return javascript_to_string(&Value::Number(value));
    }
    let value = if value == 0.0 { 0.0 } else { value };
    let raw = match fraction {
        Some(fraction) => format!("{value:.fraction$e}"),
        None => {
            let shortest = javascript_to_string(&Value::Number(value));
            let parsed = shortest.parse::<f64>().unwrap_or(value);
            format!("{parsed:e}")
        }
    };
    normalize_exponent(raw, fraction)
}

fn normalize_exponent(raw: String, fraction: Option<usize>) -> String {
    let (mantissa, exponent) = raw.split_once('e').expect("Rust exponent formatting");
    let mut mantissa = mantissa.to_string();
    if fraction.is_none() {
        while mantissa.contains('.') && mantissa.ends_with('0') {
            mantissa.pop();
        }
        if mantissa.ends_with('.') {
            mantissa.pop();
        }
    }
    let exponent = exponent.parse::<i32>().expect("Rust exponent digits");
    format!(
        "{mantissa}e{}{exponent}",
        if exponent >= 0 { "+" } else { "" }
    )
}

fn javascript_precision(value: f64, precision: usize) -> String {
    if !value.is_finite() {
        return javascript_to_string(&Value::Number(value));
    }
    let absolute = value.abs();
    let exponent = if absolute == 0.0 {
        0
    } else {
        absolute.log10().floor() as i32
    };
    if exponent >= precision as i32 || exponent < -6 {
        javascript_exponential(value, Some(precision - 1))
    } else {
        let fraction = (precision as i32 - exponent - 1).max(0) as u8;
        ryu_js::Buffer::new()
            .format_to_fixed(value, fraction)
            .to_string()
    }
}

fn flatten_array(items: &[Value], depth: usize, output: &mut Vec<Value>) {
    for item in items {
        if depth > 0 {
            match item {
                Value::List(values) | Value::Tuple(values) => {
                    flatten_array(values, depth - 1, output);
                    continue;
                }
                _ => {}
            }
        }
        output.push(item.clone());
    }
}

fn to_uint32(value: f64) -> u32 {
    if !value.is_finite() || value == 0.0 {
        0
    } else {
        value.trunc().rem_euclid(4_294_967_296.0) as u32
    }
}

fn javascript_hypot(values: &[Value]) -> f64 {
    let values = values.iter().map(javascript_to_number).collect::<Vec<_>>();
    if values.iter().any(|value| value.is_infinite()) {
        return f64::INFINITY;
    }
    if values.iter().any(|value| value.is_nan()) {
        return f64::NAN;
    }
    let scale = values
        .iter()
        .fold(0.0_f64, |scale, value| scale.max(value.abs()));
    if scale == 0.0 {
        return 0.0;
    }
    scale
        * values
            .iter()
            .map(|value| (value / scale).powi(2))
            .sum::<f64>()
            .sqrt()
}
