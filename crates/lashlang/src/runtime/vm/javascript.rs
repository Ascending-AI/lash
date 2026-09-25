use super::super::{
    ErrorKind, ensure_javascript_string_size, javascript_string_size_error, javascript_to_number,
    javascript_to_string, nullish_property_read,
};
use super::javascript_array::{
    append_flat_map_by_reference, array_iteration_source, copy_within,
    javascript_array_method_for_value,
};
pub(super) use super::javascript_number::*;
use super::javascript_static::javascript_static_stdlib;
pub(super) use super::javascript_stdlib::*;
use super::*;
use std::collections::BTreeSet;

impl<H: ExecutionHost> Vm<'_, H> {
    pub(super) fn is_truthy_for_dialect(&self, value: &Value) -> Result<bool, RuntimeError> {
        if let Value::Ref(id) = value
            && self.heap.is_javascript_vm_object(*id)?
        {
            return Ok(true);
        }
        is_truthy(value)
    }

    pub(super) fn read_dialect_field(
        &mut self,
        target: Value,
        field: &Name,
    ) -> Result<Value, RuntimeError> {
        if let Value::Ref(id) = target {
            if self.heap.is_builtin_object(id) {
                return self.heap.builtin_read(id, field.text.as_ref());
            }
            // An own `constructor` key (a record field) wins over the kind's
            // answer, exactly as it does in ECMA.
            if field.text.as_ref() == "constructor"
                && !matches!(self.heap.get(id)?, HeapObject::Record(record) if record.get("constructor").is_some())
                && let Some(name) = self.heap.javascript_constructor_of(&Value::Ref(id))?
            {
                return self.heap.builtin_value(&name);
            }
            let value = read_javascript_heap_field(&self.heap, id, field)?;
            return self.or_inherited_builtin(value, &target, &field.text);
        }
        let inherited = inline_inherited_builtin(&target, &field.text);
        if field.text.as_ref() == "constructor"
            && !matches!(&target, Value::Record(record) if record.get_symbol(field.symbol).is_some())
            && let Some(name) = self.heap.javascript_constructor_of(&target)?
        {
            return self.heap.builtin_value(&name);
        }
        let value = read_javascript_field_direct(target, field)?;
        self.or_builtin(value, inherited)
    }

    pub(super) fn read_dialect_index(
        &mut self,
        target: Value,
        index: Value,
    ) -> Result<Value, RuntimeError> {
        if let Value::Ref(id) = target {
            let key = self.heap.javascript_to_string(&index)?;
            if self.heap.is_builtin_object(id) {
                return self.heap.builtin_read(id, &key);
            }
            if key == "constructor"
                && !matches!(self.heap.get(id)?, HeapObject::Record(record) if record.get("constructor").is_some())
                && let Some(name) = self.heap.javascript_constructor_of(&Value::Ref(id))?
            {
                return self.heap.builtin_value(&name);
            }
            let value = read_javascript_heap_index(&self.heap, id, &index)?;
            if !matches!(value, Value::Undefined) {
                return Ok(value);
            }
            return self.or_inherited_builtin(value, &target, &key);
        }
        // A `null` or `undefined` base throws before its key is converted, so
        // an object key's own `toString` never runs.
        if matches!(target, Value::Null | Value::Undefined) {
            return Err(nullish_property_read(&target, &index));
        }
        let key = self.heap.javascript_to_string(&index)?;
        if key == "constructor"
            && !matches!(&target, Value::Record(record) if record.get(&key).is_some())
            && let Some(name) = self.heap.javascript_constructor_of(&target)?
        {
            return self.heap.builtin_value(&name);
        }
        let inherited = inline_inherited_builtin(&target, &key);
        let value = read_javascript_index_direct_with_key(target, &key)?;
        self.or_builtin(value, inherited)
    }

    pub(super) async fn iterable_values_for_dialect(
        &mut self,
        iterable: Value,
    ) -> Result<ListValue, RuntimeError> {
        let values = match iterable {
            Value::Ref(id) => match self.heap.get(id)? {
                HeapObject::Map(map) => map
                    .entries
                    .iter()
                    .map(|(key, value)| Value::List(vec![key.clone(), value.clone()].into()))
                    .collect::<Vec<_>>()
                    .into(),
                HeapObject::Set(set) => set.values.clone().into(),
                HeapObject::UrlSearchParams(params) => params
                    .entries
                    .iter()
                    .map(|(name, value)| {
                        Value::List(
                            vec![Value::String(name.into()), Value::String(value.into())].into(),
                        )
                    })
                    .collect::<Vec<_>>()
                    .into(),
                HeapObject::RegExp(_) | HeapObject::Date(_) | HeapObject::Url(_) => {
                    return Err(RuntimeError::ValidationFailed {
                        reason: format!(
                            "TS_FOR_OF_EXOTIC_UNSUPPORTED: {} is not iterable",
                            self.heap.get(id)?.kind_name()
                        ),
                    });
                }
                _ => {
                    let exported = self.heap.export_for_instruction(&Value::Ref(id))?;
                    // Exporting reads the iterable's whole graph once.
                    self.charge_intrinsic_work(deep_proportional_units(&exported));
                    iterable_values(exported).await?
                }
            },
            iterable => iterable_values(iterable).await?,
        };
        // Materializing the iteration writes every yielded value once.
        self.charge_intrinsic_work(values.len());
        Ok(values)
    }

    pub(super) fn execute_javascript_split(&mut self) -> Result<(), RuntimeError> {
        let separator = self.pop_stack()?;
        let value = self.pop_stack()?;
        let separator = self.heap.export_for_instruction(&separator)?;
        let value = self.heap.export_for_instruction(&value)?;
        // Splitting reads the text once and writes each part once.
        self.charge_intrinsic_work(
            proportional_units(&value).saturating_add(proportional_units(&separator)),
        );
        let values = javascript_split(&value, &separator)?;
        self.charge_intrinsic_work(values.len());
        self.stack.push(self.heap.allocate_list(values)?);
        Ok(())
    }

    pub(super) fn execute_javascript_join(&mut self) -> Result<(), RuntimeError> {
        let separator = self.pop_stack()?;
        let value = self.pop_stack()?;
        let separator = self.heap.export_for_instruction(&separator)?;
        let value = self.heap.export_for_instruction(&value)?;
        // Joining reads every member and writes each output byte once.
        self.charge_intrinsic_work(deep_proportional_units(&value));
        let joined = javascript_join(&value, &separator)?;
        self.charge_intrinsic_work(joined.len());
        self.stack.push(Value::String(joined.into()));
        Ok(())
    }

    pub(super) fn execute_javascript_stdlib(&mut self, argc: usize) -> Result<(), RuntimeError> {
        let mut values = Vec::with_capacity(argc);
        for _ in 0..argc {
            values.push(self.pop_stack()?);
        }
        values.reverse();
        if let Some(Value::String(method)) = values.first()
            && method.as_str() == "Lash.Apply"
        {
            values = self.applied_stdlib_arguments(values)?;
        }
        if let Some(source) = array_iteration_source(&self.heap, &mut values)? {
            self.stack.push(source);
            return Ok(());
        }
        // An authored member call on a plain object: the object's own
        // property is the method (ECMA-262 GetValue, then Call with the object
        // as `this`). The lowerer calls a built-in method name through here
        // only for an authored member call, and a generated helper never
        // reaches a plain-object receiver: every lowering that runs one first
        // routes a plain object to its own method (`Lash.OwnMethod`).
        if let [Value::String(method), receiver, arguments @ ..] = values.as_slice()
            && crate::ecma_stdlib::is_instance_method(method.as_str())
            && let Some(resolved) = self.plain_object_method(receiver, method.as_str())?
        {
            let name = method.to_string();
            let receiver = receiver.clone();
            let arguments = arguments.to_vec();
            return self.call_plain_object_method(resolved, &name, receiver, arguments);
        }
        self.convert_guest_arguments(&mut values)?;
        if let [Value::String(selector), key] = values.as_slice()
            && selector.as_str() == "Lash.ToPropertyKey"
        {
            // Coercing a string key reads it whole.
            self.charge_intrinsic_work(proportional_units(key));
            let key = self.heap.javascript_to_string(key)?;
            self.stack.push(Value::String(key.into()));
            return Ok(());
        }
        // Whether a lowering that runs a built-in method as generated code must
        // instead call the receiver's own member: see above.
        if let [Value::String(selector), receiver, Value::String(method)] = values.as_slice()
            && selector.as_str() == "Lash.OwnMethod"
        {
            let own = self
                .plain_object_method(receiver, method.as_str())?
                .is_some();
            self.stack.push(Value::Bool(own));
            return Ok(());
        }
        if self.execute_lash_intrinsic(&values)? {
            return Ok(());
        }
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
            // SerializeJSONProperty calls `toJSON` only when it is callable;
            // any other `toJSON` is an ordinary property.
            let has = match value {
                Value::Ref(id) => match self.heap.get(*id)? {
                    HeapObject::Record(record) => match record.get("toJSON") {
                        Some(Value::Ref(hook)) => {
                            matches!(self.heap.get(*hook)?, HeapObject::Closure { .. })
                        }
                        _ => false,
                    },
                    _ => false,
                },
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
        if let [Value::String(method), Value::Ref(output), value] = values.as_slice()
            && method.as_str() == "__appendFlatMap"
            && let Some(appended) = append_flat_map_by_reference(&self.heap, *output, value)?
        {
            // Appending copies each member the output list holds once.
            self.charge_intrinsic_work(proportional_units(&appended));
            self.stack.push(appended);
            return Ok(());
        }
        if let [Value::String(method), value] = values.as_slice()
            && method.as_str() == "__jsonHasCycle"
        {
            let mut work = 0usize;
            let has_cycle = javascript_json_has_cycle(
                &self.heap,
                value,
                &mut BTreeSet::new(),
                &mut BTreeSet::new(),
                &mut work,
            )?;
            // The walk visited each reachable object and member once.
            self.charge_intrinsic_work(work);
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
            // The membership probe scans the whole active list once.
            let present = values.iter().any(|value| value == needle);
            let scan = values.len();
            self.charge_intrinsic_work(scan);
            self.stack.push(Value::Bool(present));
            return Ok(());
        }
        if let [Value::String(method), arguments @ ..] = values.as_slice()
            && method.as_str() == "String.raw"
        {
            let result = self.javascript_string_raw(arguments)?;
            // The join writes each output byte once.
            self.charge_intrinsic_work(proportional_units(&result));
            self.stack.push(result);
            return Ok(());
        }
        if let [Value::String(method), arguments @ ..] = values.as_slice()
            && method.as_str() == javascript_substrate::CONSOLE_OBSERVATION_TEXT
        {
            let text =
                javascript_substrate::javascript_console_observation_text(&self.heap, arguments)?;
            // Rendering writes each output byte once.
            self.charge_intrinsic_work(text.len());
            self.stack.push(Value::String(text.into()));
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
                Ok(Some(json)) => {
                    // Serializing writes every byte of the text once.
                    self.charge_intrinsic_work(json.len());
                    self.stack.push(Value::String(json.into()));
                }
                Ok(None) => self.stack.push(Value::Undefined),
                Err(error) => return Err(error),
            }
            return Ok(());
        }
        if self.execute_ecma_guard(&values)? {
            return Ok(());
        }
        // `Array.isArray` asks what a heap object is, and every kind answers
        // without crossing the host boundary: an array or a match (which is
        // one) is, and a `Map`, `Set`, `Date`, `RegExp`, `URL`, error or
        // function is not.
        if let [Value::String(method), Value::Ref(receiver)] = values.as_slice()
            && method.as_str() == "Array.isArray"
        {
            let is_array = matches!(
                self.heap.get(*receiver)?,
                HeapObject::List(_) | HeapObject::Tuple(_) | HeapObject::RegExpMatch(_)
            ) || self
                .heap
                .builtin_name(*receiver)
                .is_some_and(|name| name == "Array.prototype");
            self.stack.push(Value::Bool(is_array));
            return Ok(());
        }
        if self.try_execute_regexp_match_stdlib(&values)? {
            return Ok(());
        }
        if self.execute_heap_property_intrinsic(&values)? {
            return Ok(());
        }
        if self.object_is_by_reference(&values) {
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
            && matches!(method.as_str(), "Lash.ArrayFromIterable" | "Array.from")
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
                // A shallow copy that keeps each element's identity, as
                // Array.from does; exporting the array would copy its
                // elements, and could not carry an element that holds a
                // function.
                HeapObject::List(items) | HeapObject::Tuple(items) => Some(items.clone()),
                HeapObject::RegExpMatch(result) => Some(result.items.clone()),
                _ => None,
            };
            if let Some(output) = output {
                // The shallow copy writes one element per member.
                self.charge_intrinsic_work(output.len());
                self.stack.push(Value::List(output.into()));
                return Ok(());
            }
        }
        if let [Value::String(method), args @ ..] = values.as_slice()
            && method.as_str() == "URL.canParse"
        {
            let parsed = self.execute_url_can_parse(args)?;
            self.stack.push(parsed);
            return Ok(());
        }
        if let [Value::String(method), args @ ..] = values.as_slice()
            && let Some(result) = self.execute_javascript_date_static(method, args)?
        {
            self.stack.push(result);
            return Ok(());
        }
        // The pure stdlib dispatches on value shape, so both kinds of indirection
        // have to be resolved first: a heap reference by exporting it, and a
        // projected host handle by reading the value behind it. Leaving a handle
        // here matched no receiver shape and reported the guest's method as
        // unavailable.
        for value in &mut values {
            match value {
                Value::Ref(_) => {
                    *value = self.heap.export_for_instruction(value)?;
                    // Exporting reads the argument's whole graph once.
                    self.charge_intrinsic_work(deep_proportional_units(value));
                }
                Value::Projected(_) => *value = materialize_value(value.clone())?,
                _ => {}
            }
        }
        // Array-likes are the one stdlib shape whose result size a guest names
        // outright, so they are built here rather than in `javascript_stdlib`:
        // the pure function has no heap to charge, and a `collect()` there is a
        // raw allocation of whatever `length` says.
        if let [Value::String(method), Value::Record(record)] = values.as_slice()
            && matches!(method.as_str(), "Array.from" | "Lash.ArrayFromIterable")
        {
            let elements = self.array_like_elements(record)?;
            self.stack.push(Value::List(elements.into()));
            return Ok(());
        }
        let result = javascript_stdlib(&self.heap, &values, &mut self.instructions_executed)?;
        if let Value::String(value) = &result {
            ensure_javascript_string_size(value.len())?;
        }
        self.stack.push(result);
        Ok(())
    }

    /// Materialises the dense array an array-like (`{ length, 0, 1, ... }`)
    /// denotes, charging the whole allocation before the first element exists.
    ///
    /// `length` is guest-chosen, so this is the cheapest way to name an
    /// arbitrarily large allocation in the language. Past the ECMA array limit
    /// it is `RangeError: Invalid array length`, exactly as node reports it;
    /// under the limit but over the heap budget it is the typed memory
    /// diagnostic. Neither is a clamp: silently truncating to `u32::MAX` would
    /// hand the guest an array of a length it did not ask for.
    /// `Lash.Apply(method, fixed..., arguments)`: a call with a spread
    /// argument (FIG-3627). The arguments array, built by the caller from its
    /// argument list, supplies the call's trailing arguments one by one, so
    /// `Math.max(...xs)` dispatches exactly as `Math.max(x0, x1, ...)` does.
    /// Its elements are read in place, so an object passed through a spread
    /// keeps its identity.
    fn applied_stdlib_arguments(
        &mut self,
        mut values: Vec<Value>,
    ) -> Result<Vec<Value>, RuntimeError> {
        let arguments = match values.pop() {
            Some(Value::List(items) | Value::Tuple(items)) => items.to_vec(),
            Some(Value::Ref(id)) => match self.heap.get(id)? {
                HeapObject::List(items) | HeapObject::Tuple(items) => items.clone(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        // A spread splices every element of its arguments array into the call.
        self.charge_intrinsic_work(arguments.len());
        values.remove(0);
        values.extend(arguments);
        Ok(values)
    }

    fn array_like_elements(&mut self, record: &Record) -> Result<Vec<Value>, RuntimeError> {
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
            return Err(RuntimeError::range_error("Invalid array length"));
        }
        let length = length as usize;
        self.heap.ensure_list_allocation_len(length)?;
        // The materialization writes `length` members, whatever the record holds.
        self.charge_intrinsic_work(length);
        Ok((0..length)
            .map(|index| {
                record
                    .get(&index.to_string())
                    .cloned()
                    .unwrap_or(Value::Undefined)
            })
            .collect())
    }

    #[expect(
        clippy::expect_used,
        reason = "each arm's receiver kind was checked by the match, so map_entries or set_values resolves, per each message"
    )]
    fn execute_javascript_heap_method(
        &mut self,
        method: &str,
        receiver: HeapId,
        args: &[Value],
    ) -> Result<(), RuntimeError> {
        if self.try_execute_regexp_match_method(method, receiver, args)? {
            return Ok(());
        }
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
        // Every Map or Set method but `toString`/`valueOf` scans or rewrites
        // its member list once; `forEach` queues one callback per member.
        match kind {
            "Map" if !matches!(method, "toString" | "valueOf") => {
                self.charge_intrinsic_work(self.heap.map_len(receiver)?);
            }
            "Set" if !matches!(method, "toString" | "valueOf") => {
                self.charge_intrinsic_work(self.heap.set_len(receiver)?);
            }
            _ => {}
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
                    match error
                        .message
                        .as_deref()
                        .filter(|message| !message.is_empty())
                    {
                        None => error.kind.name().to_string(),
                        Some(message) => format!("{}: {}", error.kind.name(), message),
                    }
                    .into(),
                ))
            }
            (kind, "valueOf", []) if ErrorKind::from_name(kind).is_some() => {
                Some(Value::Ref(receiver))
            }
            ("Map", "get", [key]) => {
                // The lookup scans the entries once.
                self.charge_intrinsic_work(self.heap.map_len(receiver)?);
                Some(
                    self.heap
                        .map_get(receiver, key)?
                        .unwrap_or(Value::Undefined),
                )
            }
            ("Map", "has", [key]) => {
                // The lookup scans the entries once.
                self.charge_intrinsic_work(self.heap.map_len(receiver)?);
                Some(Value::Bool(self.heap.map_has(receiver, key)?))
            }
            ("Map", "set", [key, value]) => {
                self.map_set_live(receiver, key, value)?;
                Some(Value::Ref(receiver))
            }
            ("Map", "delete", [key]) => Some(Value::Bool(self.map_delete_live(receiver, key)?)),
            ("Map", "clear", []) => {
                self.heap.map_clear(receiver)?;
                self.map_for_each_clear(receiver);
                Some(Value::Undefined)
            }
            ("Map", "keys", []) => {
                let entries = self
                    .heap
                    .map_entries(receiver)?
                    .expect("Map receiver was checked");
                // Enumerating reads and writes one result per entry.
                self.charge_intrinsic_work(entries.len());
                Some(Value::List(
                    entries
                        .into_iter()
                        .map(|(key, _)| key)
                        .collect::<Vec<_>>()
                        .into(),
                ))
            }
            ("Map", "values", []) => {
                let entries = self
                    .heap
                    .map_entries(receiver)?
                    .expect("Map receiver was checked");
                // Enumerating reads and writes one result per entry.
                self.charge_intrinsic_work(entries.len());
                Some(Value::List(
                    entries
                        .into_iter()
                        .map(|(_, value)| value)
                        .collect::<Vec<_>>()
                        .into(),
                ))
            }
            ("Map", "entries", []) => {
                let entries = self
                    .heap
                    .map_entries(receiver)?
                    .expect("Map receiver was checked");
                // Enumerating reads and writes one pair per entry.
                self.charge_intrinsic_work(entries.len());
                Some(Value::List(
                    entries
                        .into_iter()
                        .map(|(key, value)| Value::List(vec![key, value].into()))
                        .collect::<Vec<_>>()
                        .into(),
                ))
            }
            ("Map", "forEach", [function]) => {
                // The durable call queue is updated by Map mutations while the
                // callback is active. It therefore acts like ECMA's live ordered
                // entry list, including delete-and-reinsert at the tail.
                let calls: Vec<Vec<Value>> = self
                    .heap
                    .map_entries(receiver)?
                    .expect("Map receiver was checked")
                    .into_iter()
                    .map(|(key, value)| vec![value, key, Value::Ref(receiver)])
                    .collect();
                // The queue holds one call per entry.
                self.charge_intrinsic_work(calls.len());
                self.begin_callback_driver(function.clone(), calls, false, true)?;
                None
            }
            ("Set", "has", [value]) => {
                // The lookup scans the members once.
                self.charge_intrinsic_work(self.heap.set_len(receiver)?);
                Some(Value::Bool(self.heap.set_has(receiver, value)?))
            }
            ("Set", "add", [value]) => {
                self.set_add_live(receiver, value)?;
                Some(Value::Ref(receiver))
            }
            ("Set", "delete", [value]) => Some(Value::Bool(self.set_delete_live(receiver, value)?)),
            ("Set", "clear", []) => {
                self.heap.set_clear(receiver)?;
                self.set_for_each_clear(receiver);
                Some(Value::Undefined)
            }
            ("Set", "keys" | "values" | "entries", []) => {
                let values = self
                    .heap
                    .set_values(receiver)?
                    .expect("Set receiver was checked");
                // Enumerating reads and writes one result per member.
                self.charge_intrinsic_work(values.len());
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
                // As with Map, mutation maintains this durable pending queue as
                // the live ordered Set contents change.
                let calls: Vec<Vec<Value>> = self
                    .heap
                    .set_values(receiver)?
                    .expect("Set receiver was checked")
                    .into_iter()
                    .map(|value| vec![value.clone(), value, Value::Ref(receiver)])
                    .collect();
                // The queue holds one call per member.
                self.charge_intrinsic_work(calls.len());
                self.begin_callback_driver(function.clone(), calls, false, true)?;
                None
            }
            (
                "Set",
                "union"
                | "intersection"
                | "difference"
                | "symmetricDifference"
                | "isSubsetOf"
                | "isSupersetOf"
                | "isDisjointFrom",
                [other],
            ) => Some(self.execute_javascript_set_method(method, receiver, other)?),
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
    work: &mut usize,
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
        HeapObject::RegExpMatch(result) => result
            .items
            .iter()
            .chain([&result.index, &result.input, &result.groups])
            .collect(),
        HeapObject::Record(record) => record.values().collect(),
        _ => return Ok(false),
    };
    *work = work.saturating_add(children.len());
    active.insert(*id);
    for child in children {
        if javascript_json_has_cycle(heap, child, active, visited, work)? {
            return Ok(true);
        }
    }
    active.remove(id);
    visited.insert(*id);
    Ok(false)
}

pub(super) fn javascript_stdlib(
    heap: &Heap,
    values: &[Value],
    instructions_executed: &mut u64,
) -> Result<Value, RuntimeError> {
    let Some(Value::String(method)) = values.first() else {
        return Err(js_stdlib_error("missing method discriminator"));
    };
    let args = &values[1..];
    if method.as_str() == "__reduceEmpty" {
        return Err(RuntimeError::type_error(
            "Reduce of empty array with no initial value",
        ));
    }
    if method.contains('.') {
        let result = javascript_static_stdlib(method, args, instructions_executed)?;
        // Whatever the call wrote — text bytes or members — it wrote once.
        charge_collection_work(instructions_executed, proportional_units(&result));
        return Ok(result);
    }
    let Some((target, args)) = args.split_first() else {
        return Err(js_stdlib_error("missing receiver"));
    };
    let result = match target {
        Value::String(value) => {
            javascript_string_method(method, value, args, instructions_executed)?
        }
        Value::List(items) | Value::Tuple(items) => javascript_array_method_for_value(
            heap,
            method,
            target,
            items.as_ref(),
            args,
            instructions_executed,
        )?,
        Value::Number(value) => javascript_number_method(method, *value, args)?,
        // Reading a member of `null`/`undefined` is an ECMA `TypeError` about
        // the *receiver*, not a statement about the method: `globalThis.missing`
        // is `undefined`, and reporting `.get` as an unsupported method sent
        // readers looking for a missing builtin instead of at the undefined
        // value one step to the left. Named the way ECMA names it, so the
        // diagnostic matches what the guest would have seen in a browser.
        Value::Null | Value::Undefined => {
            return Err(nullish_property_read(
                target,
                &Value::String(method.clone()),
            ));
        }
        _ if method == "toString" && args.is_empty() => {
            Value::String(javascript_to_string(target).into())
        }
        _ if method == "valueOf" && args.is_empty() => target.clone(),
        _ => {
            return Err(js_stdlib_error(format!(
                "TS_METHOD_UNSUPPORTED: method `{method}` is unavailable on this value"
            )));
        }
    };
    // Whatever the method wrote — text bytes or members — it wrote once.
    charge_collection_work(instructions_executed, proportional_units(&result));
    Ok(result)
}

pub(super) fn javascript_string_method(
    method: &str,
    value: &str,
    args: &[Value],
    instructions_executed: &mut u64,
) -> Result<Value, RuntimeError> {
    use crate::runtime::javascript::javascript_to_number;
    let args = normalized_instance_arguments(method, args);
    let units = value.encode_utf16().collect::<Vec<_>>();
    // Every string method reads the input's code units once.
    charge_collection_work(instructions_executed, units.len());
    match (method, args.as_slice()) {
        ("at", [index]) => {
            let index = relative_index(javascript_to_number(index), units.len());
            index.map_or(Ok(Value::Undefined), |index| {
                utf16_value(vec![units[index]])
            })
        }
        ("charAt", [index]) => relative_nonnegative_index(javascript_to_number(index), units.len())
            .map_or(Ok(Value::String("".into())), |index| {
                utf16_value(vec![units[index]])
            }),
        ("charCodeAt", [index]) => Ok(Value::Number(
            relative_nonnegative_index(javascript_to_number(index), units.len())
                .map_or(f64::NAN, |index| units[index] as f64),
        )),
        ("codePointAt", [index]) => {
            relative_nonnegative_index(javascript_to_number(index), units.len())
                .map_or(Ok(Value::Undefined), |index| code_point_at(&units, index))
        }
        ("concat", values) => {
            let values = values.iter().map(javascript_to_string).collect::<Vec<_>>();
            // Converting each argument writes its text once.
            charge_collection_work(
                instructions_executed,
                values
                    .iter()
                    .fold(0usize, |total, item| total.saturating_add(item.len())),
            );
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
        ("startsWith", [needle, position]) => {
            // The comparison converts and reads the needle once.
            charge_collection_work(
                instructions_executed,
                javascript_to_string(needle).encode_utf16().count(),
            );
            string_starts_with(
                &units,
                needle,
                clamp_nonnegative_index(javascript_to_number(position), units.len()),
            )
        }
        ("endsWith", [needle, Value::Undefined]) => {
            charge_collection_work(
                instructions_executed,
                javascript_to_string(needle).encode_utf16().count(),
            );
            string_ends_with(&units, needle, units.len())
        }
        ("endsWith", [needle, position]) => {
            charge_collection_work(
                instructions_executed,
                javascript_to_string(needle).encode_utf16().count(),
            );
            string_ends_with(
                &units,
                needle,
                clamp_nonnegative_index(javascript_to_number(position), units.len()),
            )
        }
        ("includes", [needle, position]) => {
            // A substring scan can compare the needle at every position.
            charge_collection_work(
                instructions_executed,
                units
                    .len()
                    .saturating_mul(javascript_to_string(needle).encode_utf16().count().max(1)),
            );
            string_includes(
                &units,
                needle,
                clamp_nonnegative_index(javascript_to_number(position), units.len()),
            )
        }
        ("indexOf", [needle, position]) => {
            charge_collection_work(
                instructions_executed,
                units
                    .len()
                    .saturating_mul(javascript_to_string(needle).encode_utf16().count().max(1)),
            );
            string_index_of(
                &units,
                needle,
                clamp_nonnegative_index(javascript_to_number(position), units.len()),
            )
        }
        ("lastIndexOf", [needle, Value::Undefined]) => {
            charge_collection_work(
                instructions_executed,
                units
                    .len()
                    .saturating_mul(javascript_to_string(needle).encode_utf16().count().max(1)),
            );
            string_last_index_of(&units, needle, units.len())
        }
        ("lastIndexOf", [needle, position]) => {
            charge_collection_work(
                instructions_executed,
                units
                    .len()
                    .saturating_mul(javascript_to_string(needle).encode_utf16().count().max(1)),
            );
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
        ("padStart", [length, Value::Undefined]) => {
            pad_string(value, javascript_to_number(length), " ", true)
        }
        ("padStart", [length, fill]) => {
            let fill = javascript_to_string(fill);
            charge_collection_work(instructions_executed, fill.len());
            pad_string(value, javascript_to_number(length), &fill, true)
        }
        ("padEnd", [length, Value::Undefined]) => {
            pad_string(value, javascript_to_number(length), " ", false)
        }
        ("padEnd", [length, fill]) => {
            let fill = javascript_to_string(fill);
            charge_collection_work(instructions_executed, fill.len());
            pad_string(value, javascript_to_number(length), &fill, false)
        }
        ("repeat", [count]) => {
            let count = javascript_to_number(count);
            let count = if count.is_nan() { 0.0 } else { count };
            if !count.is_finite() || count < 0.0 {
                return Err(RuntimeError::range_error(format!(
                    "Invalid count value: {}",
                    javascript_to_string(&Value::Number(count))
                )));
            }
            let count = count.trunc() as usize;
            let output_bytes = value
                .len()
                .checked_mul(count)
                .ok_or_else(|| javascript_string_size_error(usize::MAX))?;
            ensure_javascript_string_size(output_bytes)?;
            Ok(Value::String(value.repeat(count).into()))
        }
        ("replace", [needle, replacement]) => {
            let needle = javascript_to_string(needle);
            let replacement = javascript_to_string(replacement);
            // The scan converts and reads both operands once.
            charge_collection_work(
                instructions_executed,
                needle.len().saturating_add(replacement.len()),
            );
            replace_string(value, &needle, &replacement).map(|value| Value::String(value.into()))
        }
        ("replaceAll", [needle, replacement]) => {
            let needle = javascript_to_string(needle);
            let replacement = javascript_to_string(replacement);
            charge_collection_work(
                instructions_executed,
                needle.len().saturating_add(replacement.len()),
            );
            replace_all_string(value, &needle, &replacement)
                .map(|value| Value::String(value.into()))
        }
        ("slice", bounds) => slice_utf16(&units, bounds, true),
        ("substring", bounds) => substring_utf16(&units, bounds),
        ("split", []) => Ok(Value::List(vec![Value::String(value.into())].into())),
        ("split", [separator]) => {
            charge_collection_work(instructions_executed, proportional_units(separator));
            Ok(Value::List(
                javascript_split(&Value::String(value.into()), separator)?.into(),
            ))
        }
        ("split", [separator, limit]) => {
            charge_collection_work(instructions_executed, proportional_units(separator));
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

pub(super) fn javascript_array_method(
    heap: &Heap,
    method: &str,
    items: &[Value],
    args: &[Value],
    instructions_executed: &mut u64,
) -> Result<Value, RuntimeError> {
    use crate::runtime::javascript::javascript_to_number;
    let argument_count = args.len();
    let args = normalized_instance_arguments(method, args);
    // Every array method reads its elements once.
    charge_collection_work(instructions_executed, items.len());
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
        ("copyWithin", _) => {
            let mut values = items.to_vec();
            copy_within(&mut values, &args);
            Ok(Value::List(values.into()))
        }
        ("concat", values) => {
            // Each argument list is copied member by member.
            charge_collection_work(
                instructions_executed,
                values.iter().fold(0usize, |total, value| {
                    total.saturating_add(proportional_units(value))
                }),
            );
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
        ("includes", [needle, from]) => array_includes(
            items,
            needle,
            clamp_relative_index(heap.javascript_to_number(from)?, items.len()),
        ),
        ("indexOf", [needle, from]) => array_index_of(
            items,
            needle,
            clamp_relative_index(heap.javascript_to_number(from)?, items.len()),
        ),
        ("lastIndexOf", [needle, Value::Undefined]) if argument_count < 2 => {
            array_last_index_of(items, needle, items.len())
        }
        ("lastIndexOf", [needle, from]) => {
            last_index_exclusive(heap.javascript_to_number(from)?, items.len())
                .map_or(Ok(Value::Number(-1.0)), |end| {
                    array_last_index_of(items, needle, end)
                })
        }
        ("join", [separator]) => {
            // Joining reads every element's text.
            charge_collection_work(
                instructions_executed,
                items.iter().fold(0usize, |total, value| {
                    total.saturating_add(proportional_units(value))
                }),
            );
            Ok(Value::String(
                javascript_join(&Value::List(items.to_vec().into()), separator)?.into(),
            ))
        }
        ("flat", depth) => {
            // Omitted and explicit `undefined` both select the ECMA default
            // depth 1; coercing the padded `Undefined` to a number would give
            // NaN and silently flatten nothing.
            let depth = match depth.first() {
                None | Some(Value::Undefined) => 1,
                Some(value) => {
                    let depth = javascript_to_number(value);
                    if depth.is_nan() || depth <= 0.0 {
                        0
                    } else if depth == f64::INFINITY {
                        usize::MAX
                    } else {
                        depth.trunc() as usize
                    }
                }
            };
            // Flattening descends through every nested member.
            charge_collection_work(
                instructions_executed,
                items.iter().fold(0usize, |total, value| {
                    total.saturating_add(deep_proportional_units(value))
                }),
            );
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
        ("toString", []) => {
            charge_collection_work(
                instructions_executed,
                items.iter().fold(0usize, |total, value| {
                    total.saturating_add(proportional_units(value))
                }),
            );
            Ok(Value::String(
                javascript_join(&Value::List(items.to_vec().into()), &Value::Undefined)?.into(),
            ))
        }
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
