//! One JSON traversal over borrowed live values or copied-out data.
use super::*;

/// The only difference between TS host transport and TS JSON text is functions:
/// host transport writes the existing canonical `$lash_function` tag, while text
/// omits functions in objects and writes null for them in arrays. Undefined,
/// holes, null, ordering and path diagnostics use the same traversal for both.
/// Host function transport also retains the documented reserved-key escaping,
/// so a guest record cannot forge a `$lash_function` tag. The tag shape is unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonTarget {
    Host,
    Text,
}

/// The source representation, independent of the host/text function target.
/// JavaScript uses an empty tuple for a sparse slot and tagged tuples for
/// builtins and BigInt. Kernel/Python tuples are ordinary JSON arrays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonModel {
    JavaScript,
    Kernel,
}
impl JsonModel {
    pub fn for_dialect(dialect: &str) -> Self {
        if dialect == "typescript" {
            Self::JavaScript
        } else {
            Self::Kernel
        }
    }
}

pub(super) enum Numeric {
    Int(Integer),
    Float(Float),
    Token(NumberToken),
}

pub(super) enum View<V> {
    Null,
    Bool(bool),
    Number(Numeric),
    Text(String),
    Absent,
    Sequence(Vec<V>, &'static str),
    Mapping(Vec<(V, V)>),
    Object(Vec<(String, V)>, &'static str),
    Function,
    Unsupported(&'static str),
}

pub(super) trait Source {
    type Value: Clone;
    type Identity: PartialEq;
    #[cfg(test)]
    fn scratch(
        &self,
        _: &View<Self::Value>,
    ) -> Result<crate::native_contract_laws::ScratchLease, NativeError> {
        Ok(crate::native_contract_laws::scratch(0))
    }
    fn javascript(&self) -> bool {
        true
    }
    fn function(&self, _: &Self::Value) -> Result<Option<String>, NativeError> {
        Ok(None)
    }
    fn view(
        &self,
        value: &Self::Value,
        output: &mut impl Output,
    ) -> Result<View<Self::Value>, NativeError>;
    fn identity(&self, value: &Self::Value) -> Option<Self::Identity>;
}

pub(super) struct Live<'call>(pub Args<'call>);
impl Source for Live<'_> {
    type Value = Value;
    type Identity = ArenaHandle;
    fn view(&self, value: &Value, output: &mut impl Output) -> Result<View<Value>, NativeError> {
        Ok(match value {
            Value::Null => View::Null,
            Value::Bool(value) => View::Bool(*value),
            Value::Absent => View::Absent,
            Value::Int(_) | Value::BigInt(_) => View::Number(Numeric::Int(
                self.0
                    .integer(value)?
                    .ok_or(NativeError::Malformed("JSON integer body"))?
                    .into_owned(),
            )),
            Value::Float(value) => View::Number(Numeric::Float(*value)),
            Value::Text(_) | Value::LongText(_) => {
                let text = self
                    .0
                    .text(value)?
                    .ok_or(NativeError::Malformed("JSON text body"))?;
                output.read_whole(text.len())?;
                View::Text(text.whole()?.into_owned())
            }
            Value::List(_) | Value::Tuple(_) => View::Sequence(
                super::super::sequence_values(&self.0, value)?,
                if matches!(value, Value::List(_)) {
                    "list"
                } else {
                    "tuple"
                },
            ),
            Value::Map(_) => {
                let table = self
                    .0
                    .table(value)?
                    .ok_or(NativeError::Malformed("JSON map body"))?;
                let mut fields = Vec::with_capacity(table.len() as usize);
                for chunk in table.chunks() {
                    for pair in chunk?.as_chunks::<2>().0 {
                        fields.push((pair[0], pair[1]));
                    }
                }
                View::Mapping(fields)
            }
            Value::Record(_) => {
                let table = self
                    .0
                    .table(value)?
                    .ok_or(NativeError::Malformed("JSON table body"))?;
                let mut fields = Vec::with_capacity(table.len() as usize);
                for chunk in table.chunks() {
                    for pair in chunk?.as_chunks::<2>().0 {
                        let key = self
                            .0
                            .text(&pair[0])?
                            .ok_or_else(|| raise("json_key", "JSON map keys must be text"))?;
                        output.read_whole(key.len())?;
                        fields.push((key.whole()?.into_owned(), pair[1]));
                    }
                }
                View::Object(
                    fields,
                    if matches!(value, Value::Record(_)) {
                        "record"
                    } else {
                        "map"
                    },
                )
            }
            Value::Closure(_) | Value::Function(_) => View::Function,
            Value::Bytes(_) => View::Unsupported("bytes"),
            Value::Timestamp(_) | Value::BigTimestamp(_) => View::Unsupported("timestamp"),
            Value::Set(_) => View::Unsupported("set"),
            Value::Error(_) => View::Unsupported("error"),
            Value::Task(_) | Value::TaskRef(_) => View::Unsupported("task"),
            Value::Handle(_) => View::Unsupported("handle"),
            Value::ObjectRef(_) => View::Unsupported("ref"),
        })
    }
    #[cfg(test)]
    fn scratch(
        &self,
        view: &View<Value>,
    ) -> Result<crate::native_contract_laws::ScratchLease, NativeError> {
        let (values, names) = match view {
            View::Sequence(values, _) => (values.clone(), 0),
            View::Mapping(fields) => (
                fields
                    .iter()
                    .flat_map(|(key, value)| [*key, *value])
                    .collect(),
                0,
            ),
            View::Object(fields, _) => (
                fields.iter().map(|(_, value)| *value).collect(),
                fields.iter().map(|(key, _)| key.len() as u64).sum(),
            ),
            _ => (Vec::new(), 0),
        };
        let mut lease = crate::native_contract_laws::try_scratch_values(self.0, &values)?;
        lease.grow(names);
        Ok(lease)
    }
    fn identity(&self, value: &Value) -> Option<ArenaHandle> {
        value.handle()
    }
}

struct Data<'a>(JsonModel, std::marker::PhantomData<&'a Datum>);
impl<'a> Source for Data<'a> {
    type Value = &'a Datum;
    fn javascript(&self) -> bool {
        self.0 == JsonModel::JavaScript
    }
    type Identity = usize;
    fn function(&self, value: &&'a Datum) -> Result<Option<String>, NativeError> {
        let Datum::FunctionValue(function) = value else {
            return Ok(None);
        };
        let bytes = function
            .canonical_tagged_bytes()
            .map_err(|error| raise("not_data", error.to_string()))?;
        Ok(Some(String::from_utf8(bytes).map_err(|_| {
            NativeError::Malformed("canonical function JSON is not UTF-8")
        })?))
    }
    fn view(&self, value: &&'a Datum, _: &mut impl Output) -> Result<View<&'a Datum>, NativeError> {
        Ok(match value {
            Datum::Null => View::Null,
            Datum::Absent => View::Absent,
            Datum::Bool(value) => View::Bool(*value),
            Datum::Int(value) => View::Number(Numeric::Int(value.clone())),
            Datum::Float(value) => View::Number(Numeric::Float(*value)),
            Datum::Number(value) => View::Number(Numeric::Token(value.clone())),
            Datum::Text(value) => View::Text(value.clone()),
            Datum::List(values) | Datum::Tuple(values) => View::Sequence(
                values.iter().collect(),
                if matches!(value, Datum::List(_)) {
                    "list"
                } else {
                    "tuple"
                },
            ),
            Datum::Record(fields) => View::Object(
                fields
                    .iter()
                    .map(|(key, value)| (key.clone(), value))
                    .collect(),
                "record",
            ),
            Datum::Map(fields) => {
                View::Mapping(fields.iter().map(|(key, value)| (key, value)).collect())
            }
            Datum::FunctionValue(_) | Datum::Function(_) => View::Function,
            Datum::Bytes(_) => View::Unsupported("bytes"),
            Datum::Timestamp(_) => View::Unsupported("timestamp"),
            Datum::Set(_) => View::Unsupported("set"),
            Datum::Error(_) => View::Unsupported("error"),
            Datum::Handle(_) => View::Unsupported("handle"),
        })
    }
    fn identity(&self, _: &&'a Datum) -> Option<usize> {
        None
    }
}

fn at(error: NativeError, context: &str, path: &str) -> NativeError {
    match error {
        NativeError::Refused { kind, message } => NativeError::Refused {
            kind,
            message: format!("{context} {path}: {message}"),
        },
        other => other,
    }
}
fn error(kind: &'static str, context: &str, path: &str, message: impl AsRef<str>) -> NativeError {
    at(raise(kind, message.as_ref()), context, path)
}
fn member_path(parent: &str, key: &str) -> Result<String, NativeError> {
    let identifier = !key.is_empty()
        && key.chars().enumerate().all(|(index, c)| {
            c == '_' || c == '$' || c.is_ascii_alphabetic() || (index > 0 && c.is_ascii_digit())
        });
    if identifier {
        Ok(format!("{parent}.{key}"))
    } else {
        // Keys use the same escaping routine as values.
        let mut out = Out {
            output: StringOutput(String::new()),
            args: None,
            layout: None,
            verbatim_kinds: 0,
            verbatim_field: None,
            encoding: None,
            indentation: None,
        };
        out.quote(key)?;
        Ok(format!("{parent}[{}]", out.output.0))
    }
}
fn text<A: Source>(
    source: &A,
    value: &A::Value,
    output: &mut impl Output,
) -> Result<Option<String>, NativeError> {
    Ok(match source.view(value, output)? {
        View::Text(text) => Some(text),
        _ => None,
    })
}
fn omitted<V>(view: &View<V>, target: Option<JsonTarget>) -> bool {
    target.is_some() && matches!(view, View::Absent)
        || target == Some(JsonTarget::Text) && matches!(view, View::Function)
}

fn normalize<A: Source>(
    source: &A,
    mut view: View<A::Value>,
    out: &mut Out<'_, '_, impl Output>,
    policy: &Policy<'_>,
) -> Result<View<A::Value>, NativeError> {
    if let View::Mapping(fields) = view {
        let mut named = Vec::with_capacity(fields.len());
        for (key, value) in fields {
            let view = source.view(&key, &mut out.output)?;
            let name = match view {
                View::Text(text) => text,
                View::Number(_) | View::Bool(_) | View::Null if out.encoding.is_some() => {
                    let mut key_out = Out {
                        output: StringOutput(String::new()),
                        args: None,
                        layout: out.layout,
                        verbatim_kinds: 0,
                        verbatim_field: None,
                        encoding: out.encoding.clone(),
                        indentation: None,
                    };
                    Encoder {
                        source,
                        out: &mut key_out,
                        target: None,
                        context: policy.context,
                        limit: policy.limit,
                        json_number: policy.json_number,
                        active: Vec::new(),
                    }
                    .walk(key, view, 0, true, policy.path)?;
                    let rendered = key_out.output.0;
                    out.output.reserve(rendered.len() as u64)?;
                    rendered
                }
                _ if out.encoding.as_ref().is_some_and(|style| style.skipkeys) => continue,
                _ => {
                    return Err(raise(
                        "json_key",
                        "JSON map keys must be text, or Python scalar keys for json.encode",
                    ));
                }
            };
            named.push((name, value));
        }
        view = View::Object(named, "map");
    }
    if policy.target.is_some()
        && source.javascript()
        && let View::Sequence(items, "tuple") = &view
    {
        if items.is_empty() {
            view = View::Absent;
        } else {
            match text(source, &items[0], &mut out.output)?.as_deref() {
                Some("ts.function") => view = View::Function,
                Some("ts.bigint") => view = View::Unsupported("BigInt"),
                Some("ts.object") => {
                    let prototype = if items.len() > 1 {
                        text(source, &items[1], &mut out.output)?
                    } else {
                        None
                    };
                    view = match prototype.as_deref() {
                        Some("Date.prototype") => View::Null,
                        Some("Array.prototype") => View::Sequence(Vec::new(), "list"),
                        Some("String.prototype") => View::Text(String::new()),
                        Some("Number.prototype") => View::Number(Numeric::Float(Float::new(0.0))),
                        Some("Boolean.prototype") => View::Bool(false),
                        _ => View::Object(Vec::new(), "record"),
                    };
                }
                _ => {}
            }
        }
    }
    if policy.target.is_some() && source.javascript() {
        if let View::Number(Numeric::Int(integer)) = &view {
            view = View::Number(Numeric::Float(Float::new(integer.to_f64().unwrap_or(
                if integer.is_negative() {
                    f64::NEG_INFINITY
                } else {
                    f64::INFINITY
                },
            ))));
        }
        if matches!(view, View::Unsupported("error" | "set")) {
            view = View::Object(Vec::new(), "record");
        }
        if let View::Object(fields, "record") = &view
            && let Some((_, brand)) = fields.iter().find(|(key, _)| key == "__lash_brand")
            && let View::Sequence(parts, "tuple") = source.view(brand, &mut out.output)?
            && parts.len() == 2
            && text(source, &parts[0], &mut out.output)?.as_deref() == Some("typescript")
            && text(source, &parts[1], &mut out.output)?.as_deref() == Some("template")
            && let Some((_, items)) = fields.iter().find(|(key, _)| key == "items")
        {
            view = source.view(items, &mut out.output)?;
        }
    }
    Ok(view)
}

/// All structure, omission, ordering, cycle checks and JSON paths are decided here.
struct Encoder<'a, 'out, 'args, 'call, A: Source, S: Output> {
    source: &'a A,
    out: &'out mut Out<'args, 'call, S>,
    target: Option<JsonTarget>,
    context: &'a str,
    limit: usize,
    json_number: bool,
    active: Vec<A::Identity>,
}

pub(super) struct Policy<'a> {
    pub target: Option<JsonTarget>,
    pub context: &'a str,
    pub path: &'a str,
    pub limit: usize,
    /// Spell finite floats as [`Float::json_number`] does: the host's one
    /// number spelling, for copied-out data ([`encode_datum`]). Guest JSON
    /// text spells them by the caller's decimal layout instead.
    pub json_number: bool,
}

pub(super) fn encode<A: Source>(
    source: &A,
    value: A::Value,
    depth: usize,
    raw_numbers: bool,
    out: &mut Out<'_, '_, impl Output>,
    policy: Policy<'_>,
) -> Result<bool, NativeError> {
    let view = source
        .view(&value, &mut out.output)
        .and_then(|view| normalize(source, view, out, &policy))
        .map_err(|e| at(e, policy.context, policy.path))?;
    Encoder {
        source,
        out,
        target: policy.target,
        context: policy.context,
        limit: policy.limit,
        json_number: policy.json_number,
        active: Vec::new(),
    }
    .walk(value, view, depth, raw_numbers, policy.path)
}

impl<A: Source, S: Output> Encoder<'_, '_, '_, '_, A, S> {
    fn walk(
        &mut self,
        value: A::Value,
        view: View<A::Value>,
        depth: usize,
        raw_numbers: bool,
        path: &str,
    ) -> Result<bool, NativeError> {
        let source = self.source;
        let target = self.target;
        let context = self.context;
        let limit = self.limit;
        #[cfg(test)]
        let _frame = crate::native_contract_laws::scratch(4 * 16);
        #[cfg(test)]
        let _view = source.scratch(&view)?;
        if depth > limit {
            return Err(error(
                "json_depth",
                context,
                path,
                format!("JSON exceeds the configured {limit}-level kernel nesting limit"),
            ));
        }
        if omitted(&view, target) {
            return Ok(false);
        }
        match view {
            View::Null => self.out.push("null")?,
            View::Bool(value) => self.out.push(if value { "true" } else { "false" })?,
            View::Number(Numeric::Float(number)) if !number.get().is_finite() => {
                if target.is_some() {
                    self.out.push("null")?;
                } else if self
                    .out
                    .encoding
                    .as_ref()
                    .is_some_and(|policy| policy.nonfinite)
                {
                    self.out.push(if number.get().is_nan() {
                        "NaN"
                    } else if number.get().is_sign_negative() {
                        "-Infinity"
                    } else {
                        "Infinity"
                    })?;
                } else {
                    return Err(error(
                        "json_number",
                        context,
                        path,
                        "JSON refuses NaN and infinity",
                    ));
                }
            }
            View::Number(Numeric::Float(number)) if self.json_number => {
                let number = number
                    .json_number()
                    .ok_or(NativeError::Malformed("a finite float has a JSON number"))?;
                self.out.push(&number.to_string())?;
            }
            View::Number(number) => self
                .out
                .number(
                    &number,
                    raw_numbers
                        && (target.is_none()
                            || source.javascript()
                            || !matches!(number, Numeric::Int(_))),
                )
                .map_err(|e| at(e, context, path))?,
            View::Text(text) => self.out.quote(&text)?,
            View::Absent => {
                return Err(error(
                    "not_data",
                    context,
                    path,
                    "undefined has no root JSON representation",
                ));
            }
            View::Function => {
                let function = if target == Some(JsonTarget::Host) {
                    source
                        .function(&value)
                        .map_err(|error| at(error, context, path))?
                } else {
                    None
                };
                if let Some(json) = function {
                    self.out.push(&json)?;
                } else {
                    return Err(error(
                        "not_data",
                        context,
                        path,
                        "a function has no durable transport value",
                    ));
                }
            }
            View::Unsupported(kind) => {
                return Err(error(
                    "not_data",
                    context,
                    path,
                    format!("{kind} has no JSON representation"),
                ));
            }
            View::Sequence(items, kind) => {
                let identity = source.identity(&value);
                if identity.as_ref().is_some_and(|id| self.active.contains(id)) {
                    return Err(error("cycle", context, path, "cyclic value cannot be JSON"));
                }
                if let Some(id) = identity {
                    self.active.push(id);
                }
                self.out.push("[")?;
                let raw = self.out.raw_numbers(kind, raw_numbers);
                let nonempty = !items.is_empty();
                for (index, item) in items.into_iter().enumerate() {
                    if index > 0 {
                        self.out.separator(true)?;
                    }
                    self.out.newline(depth + 1)?;
                    let child = format!("{path}[{index}]");
                    let view = source
                        .view(&item, &mut self.out.output)
                        .and_then(|view| {
                            normalize(
                                source,
                                view,
                                self.out,
                                &Policy {
                                    target,
                                    context,
                                    path: &child,
                                    limit,
                                    json_number: self.json_number,
                                },
                            )
                        })
                        .map_err(|e| at(e, context, &child))?;
                    if !self.walk(item, view, depth + 1, raw, &child)? {
                        self.out.push("null")?;
                    }
                }
                if nonempty {
                    self.out.newline(depth)?;
                }
                self.out.push("]")?;
                if source.identity(&value).is_some() {
                    self.active.pop();
                }
            }
            View::Mapping(_) => return Err(NativeError::Malformed("JSON map was not normalized")),
            View::Object(mut fields, kind) => {
                let identity = source.identity(&value);
                if identity.as_ref().is_some_and(|id| self.active.contains(id)) {
                    return Err(error("cycle", context, path, "cyclic value cannot be JSON"));
                }
                if let Some(id) = identity {
                    self.active.push(id);
                }
                let branded =
                    target.is_some() && fields.iter().any(|(key, _)| key == "__lash_brand");
                if target.is_some() && source.javascript() && kind == "record" {
                    fields.sort_by_key(|(key, _)| {
                        key.parse::<u32>()
                            .ok()
                            .filter(|index| *index < u32::MAX && index.to_string() == *key)
                            .map_or((1, 0), |index| (0, index))
                    });
                }
                let mut raw = self.out.raw_numbers(kind, raw_numbers);
                if let Some(verbatim) = &self.out.verbatim_field {
                    for (key, value) in &fields {
                        if kind == "record"
                            && key == verbatim
                            && text(source, value, &mut self.out.output)?.is_some()
                        {
                            raw = false;
                            break;
                        }
                    }
                }
                self.out.push("{")?;
                let mut written = 0;
                for (key, item) in fields {
                    if branded && matches!(key.as_str(), "__lash_brand" | "__lash_built") {
                        continue;
                    }
                    let child = member_path(path, &key)?;
                    let view = source
                        .view(&item, &mut self.out.output)
                        .and_then(|view| {
                            normalize(
                                source,
                                view,
                                self.out,
                                &Policy {
                                    target,
                                    context,
                                    path: &child,
                                    limit,
                                    json_number: self.json_number,
                                },
                            )
                        })
                        .map_err(|e| at(e, context, &child))?;
                    if omitted(&view, target) {
                        continue;
                    }
                    if self.out.encoding.as_ref().is_some_and(|policy| {
                        kind == "record" && policy.excluded.iter().any(|field| field == &key)
                    }) {
                        return Err(error(
                            "not_data",
                            context,
                            &child,
                            "record has an excluded field",
                        ));
                    }
                    if written > 0 {
                        self.out.separator(true)?;
                    }
                    self.out.newline(depth + 1)?;
                    let key = if target == Some(JsonTarget::Host) {
                        lash_kernel_doc::escape_function_key(&key).into_owned()
                    } else {
                        key
                    };
                    self.out.quote(&key)?;
                    self.out.separator(false)?;
                    if self.out.indentation.is_some() && self.out.encoding.is_none() {
                        self.out.push(" ")?;
                    }
                    self.walk(item, view, depth + 1, raw, &child)?;
                    written += 1;
                }
                if written > 0 {
                    self.out.newline(depth)?;
                }
                self.out.push("}")?;
                if source.identity(&value).is_some() {
                    self.active.pop();
                }
            }
        }
        Ok(true)
    }
}

struct StringOutput(String);
impl Output for StringOutput {
    fn push(&mut self, text: &str) -> Result<(), NativeError> {
        self.0.push_str(text);
        Ok(())
    }
    fn reserve(&mut self, _: u64) -> Result<(), NativeError> {
        Ok(())
    }
    fn read_whole(&mut self, _: u64) -> Result<(), NativeError> {
        Ok(())
    }
}

/// Encode copied-out guest data using the very same traversal as `json.boundary`.
/// A required JSON document refuses an undefined root. The target changes only
/// function transport, as documented on [`JsonTarget`]. A finite float is
/// spelled as [`Float::json_number`] does, the one spelling of every host
/// reading (a tool's arguments, a cell's values): a whole float below 2^53 is
/// a JSON integer, and `2**53` stays `9007199254740992.0`. No guest value is
/// mutated.
pub fn encode_datum(
    value: &Datum,
    model: JsonModel,
    target: JsonTarget,
    context: &str,
    limit: usize,
) -> Result<String, NativeError> {
    let mut out = Out {
        output: StringOutput(String::new()),
        args: None,
        layout: None,
        verbatim_kinds: 0,
        verbatim_field: None,
        encoding: None,
        indentation: None,
    };
    let written = encode(
        &Data(model, std::marker::PhantomData),
        value,
        0,
        true,
        &mut out,
        Policy {
            target: Some(target),
            context,
            path: "$",
            limit,
            json_number: true,
        },
    )?;
    if written {
        Ok(out.output.0)
    } else {
        Err(error(
            "not_data",
            context,
            "$",
            "undefined has no root JSON representation",
        ))
    }
}
