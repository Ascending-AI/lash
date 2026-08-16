use super::javascript::js_stdlib_error;
use super::*;
use crate::runtime::ensure_javascript_string_size;

impl<H: ExecutionHost> Vm<'_, H> {
    pub(super) fn execute_url_can_parse(&self, args: &[Value]) -> Result<Value, RuntimeError> {
        let result = match args {
            [input] => {
                let input = self.heap.javascript_to_string(input)?;
                ensure_javascript_string_size(input.len())?;
                crate::runtime::heap::parse_url(&input, None).is_ok()
            }
            [input, base] => {
                let input = self.heap.javascript_to_string(input)?;
                ensure_javascript_string_size(input.len())?;
                if matches!(base, Value::Undefined) {
                    crate::runtime::heap::parse_url(&input, None).is_ok()
                } else {
                    let base = self.heap.javascript_to_string(base)?;
                    ensure_javascript_string_size(base.len())?;
                    crate::runtime::heap::parse_url(&input, Some(&base)).is_ok()
                }
            }
            _ => {
                return Err(js_stdlib_error(format!(
                    "TS_METHOD_UNSUPPORTED: URL.canParse with {} argument(s)",
                    args.len()
                )));
            }
        };
        Ok(Value::Bool(result))
    }

    pub(super) fn execute_url_heap_method(
        &mut self,
        kind: &str,
        method: &str,
        receiver: HeapId,
        args: &[Value],
    ) -> Result<Option<Value>, RuntimeError> {
        let result = match (kind, method, args) {
            ("URL", "toString" | "toJSON", []) => {
                let HeapObject::Url(url) = self.heap.get(receiver)? else {
                    unreachable!("URL receiver kind was checked")
                };
                Some(Value::String(url.href.as_str().into()))
            }
            ("URL", "valueOf", []) | ("URLSearchParams", "valueOf", []) => {
                Some(Value::Ref(receiver))
            }
            ("URLSearchParams", "toString", []) => {
                let entries = self
                    .heap
                    .url_search_params_entries(receiver)?
                    .expect("URLSearchParams receiver was checked");
                Some(Value::String(
                    crate::runtime::heap::serialize_params(&entries).into(),
                ))
            }
            ("URLSearchParams", "get", [name]) => {
                let name = self.heap.javascript_to_string(name)?;
                let value = self
                    .heap
                    .url_search_params_entries(receiver)?
                    .expect("URLSearchParams receiver was checked")
                    .into_iter()
                    .find_map(|(candidate, value)| (candidate == name).then_some(value));
                Some(value.map_or(Value::Null, |value| Value::String(value.into())))
            }
            ("URLSearchParams", "getAll", [name]) => {
                let name = self.heap.javascript_to_string(name)?;
                Some(Value::List(
                    self.heap
                        .url_search_params_entries(receiver)?
                        .expect("URLSearchParams receiver was checked")
                        .into_iter()
                        .filter_map(|(candidate, value)| {
                            (candidate == name).then_some(Value::String(value.into()))
                        })
                        .collect::<Vec<_>>()
                        .into(),
                ))
            }
            ("URLSearchParams", "append", [name, value]) => {
                let name = self.heap.javascript_to_string(name)?;
                let value = self.heap.javascript_to_string(value)?;
                self.heap
                    .url_search_params_mutate(receiver, |entries| entries.push((name, value)))?;
                Some(Value::Undefined)
            }
            ("URLSearchParams", "set", [name, value]) => {
                let name = self.heap.javascript_to_string(name)?;
                let value = self.heap.javascript_to_string(value)?;
                self.heap.url_search_params_mutate(receiver, |entries| {
                    if let Some(first) =
                        entries.iter().position(|(candidate, _)| *candidate == name)
                    {
                        entries[first].1 = value;
                        let mut index = first + 1;
                        while index < entries.len() {
                            if entries[index].0 == name {
                                entries.remove(index);
                            } else {
                                index += 1;
                            }
                        }
                    } else {
                        entries.push((name, value));
                    }
                })?;
                Some(Value::Undefined)
            }
            ("URLSearchParams", "delete", [name]) => {
                let name = self.heap.javascript_to_string(name)?;
                self.heap.url_search_params_mutate(receiver, |entries| {
                    entries.retain(|(candidate, _)| *candidate != name);
                })?;
                Some(Value::Undefined)
            }
            ("URLSearchParams", "delete", [name, value]) => {
                let name = self.heap.javascript_to_string(name)?;
                let value = self.heap.javascript_to_string(value)?;
                self.heap.url_search_params_mutate(receiver, |entries| {
                    entries.retain(|(candidate, candidate_value)| {
                        *candidate != name || *candidate_value != value
                    });
                })?;
                Some(Value::Undefined)
            }
            ("URLSearchParams", "has", [name]) => {
                let name = self.heap.javascript_to_string(name)?;
                Some(Value::Bool(
                    self.heap
                        .url_search_params_entries(receiver)?
                        .expect("URLSearchParams receiver was checked")
                        .iter()
                        .any(|(candidate, _)| *candidate == name),
                ))
            }
            ("URLSearchParams", "has", [name, value]) => {
                let name = self.heap.javascript_to_string(name)?;
                let value = self.heap.javascript_to_string(value)?;
                Some(Value::Bool(
                    self.heap
                        .url_search_params_entries(receiver)?
                        .expect("URLSearchParams receiver was checked")
                        .iter()
                        .any(|(candidate, candidate_value)| {
                            *candidate == name && *candidate_value == value
                        }),
                ))
            }
            ("URLSearchParams", "sort", []) => {
                self.heap.url_search_params_mutate(receiver, |entries| {
                    entries.sort_by(|(left, _), (right, _)| {
                        left.encode_utf16().cmp(right.encode_utf16())
                    });
                })?;
                Some(Value::Undefined)
            }
            ("URLSearchParams", "keys" | "values" | "entries", []) => {
                let entries = self
                    .heap
                    .url_search_params_entries(receiver)?
                    .expect("URLSearchParams receiver was checked");
                Some(Value::List(
                    entries
                        .into_iter()
                        .map(|(name, value)| match method {
                            "keys" => Value::String(name.into()),
                            "values" => Value::String(value.into()),
                            _ => Value::List(
                                vec![Value::String(name.into()), Value::String(value.into())]
                                    .into(),
                            ),
                        })
                        .collect::<Vec<_>>()
                        .into(),
                ))
            }
            ("URLSearchParams", "forEach", [function] | [function, _]) => {
                self.begin_url_search_params_for_each(function.clone(), receiver)?;
                None
            }
            _ => {
                return Err(js_stdlib_error(format!(
                    "TS_METHOD_UNSUPPORTED: {kind}.{method} with {} argument(s)",
                    args.len()
                )));
            }
        };
        Ok(result)
    }
}
