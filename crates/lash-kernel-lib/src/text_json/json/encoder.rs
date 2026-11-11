//! One JSON traversal over borrowed live values or copied-out data.
use super::*;
use lash_kernel_doc::arena::ShortText;
use std::borrow::Cow;

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

/// A text as the encoder reads it: a short text copied out of its value,
/// or the source's own bytes for `'k` when it holds them whole.
pub(super) enum Str<'k> {
    Inline(ShortText),
    Held(Cow<'k, str>),
}
impl std::ops::Deref for Str<'_> {
    type Target = str;
    fn deref(&self) -> &str {
        match self {
            Str::Inline(short) => short.as_str(),
            Str::Held(text) => text,
        }
    }
}
impl<'k> Str<'k> {
    fn into_cow(self) -> Cow<'k, str> {
        match self {
            Str::Inline(short) => Cow::Owned(short.as_str().to_owned()),
            Str::Held(text) => text,
        }
    }
}

/// A value as the encoder walks it. Object keys borrow from the source
/// for `'k` when it holds them whole.
pub(super) enum View<'k, V> {
    Null,
    Bool(bool),
    Number(Numeric),
    Text(Str<'k>),
    Absent,
    Sequence(Vec<V>, &'static str),
    Mapping(Vec<(V, V)>),
    Object(Vec<(Cow<'k, str>, V)>, &'static str),
    Function,
    Unsupported(&'static str),
}

pub(super) trait Source<'k> {
    type Value: Clone;
    type Identity: PartialEq;
    #[cfg(test)]
    fn scratch(
        &self,
        _: &View<'k, Self::Value>,
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
    ) -> Result<View<'k, Self::Value>, NativeError>;
    fn identity(&self, value: &Self::Value) -> Option<Self::Identity>;
}

pub(super) struct Live<'call>(pub Args<'call>);
impl<'call> Source<'call> for Live<'call> {
    type Value = Value;
    type Identity = ArenaHandle;
    fn view(
        &self,
        value: &Value,
        output: &mut impl Output,
    ) -> Result<View<'call, Value>, NativeError> {
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
            Value::Text(short) => {
                output.read_whole(short.as_bytes().len() as u64)?;
                View::Text(Str::Inline(*short))
            }
            Value::LongText(object) => {
                let text = self.0.long_text(*object)?;
                output.read_whole(text.len())?;
                View::Text(Str::Held(text.whole()?))
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
                let mut text_keys = true;
                for chunk in table.chunks() {
                    text_keys &= chunk?
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .all(|pair| matches!(pair[0], Value::Text(_) | Value::LongText(_)));
                }
                if text_keys {
                    // Every key is text: the object `normalize` would make,
                    // its keys read in the same order, without the pairs.
                    let mut fields = Vec::with_capacity(table.len() as usize);
                    for chunk in table.chunks() {
                        for pair in chunk?.as_chunks::<2>().0 {
                            let key = self
                                .0
                                .text(&pair[0])?
                                .ok_or(NativeError::Malformed("JSON map key body"))?;
                            output.read_whole(key.len())?;
                            fields.push((key.whole()?, pair[1]));
                        }
                    }
                    View::Object(fields, "map")
                } else {
                    let mut fields = Vec::with_capacity(table.len() as usize);
                    for chunk in table.chunks() {
                        for pair in chunk?.as_chunks::<2>().0 {
                            fields.push((pair[0], pair[1]));
                        }
                    }
                    View::Mapping(fields)
                }
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
                        fields.push((key.whole()?, pair[1]));
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
        view: &View<'call, Value>,
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
impl<'a> Source<'a> for Data<'a> {
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
    fn view(
        &self,
        value: &&'a Datum,
        _: &mut impl Output,
    ) -> Result<View<'a, &'a Datum>, NativeError> {
        let value: &'a Datum = value;
        Ok(match value {
            Datum::Null => View::Null,
            Datum::Absent => View::Absent,
            Datum::Bool(value) => View::Bool(*value),
            Datum::Int(value) => View::Number(Numeric::Int(value.clone())),
            Datum::Float(value) => View::Number(Numeric::Float(*value)),
            Datum::Number(value) => View::Number(Numeric::Token(value.clone())),
            Datum::Text(value) => View::Text(Str::Held(Cow::Borrowed(value))),
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
                    .map(|(key, value)| (Cow::Borrowed(key.as_str()), value))
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

/// Where a value sits in the document: a root the caller names, then an
/// index or a member per level. It is spelled only when an error names it.
#[derive(Clone, Copy)]
pub(super) enum Path<'a> {
    Root(&'a str),
    Index(&'a Path<'a>, usize),
    Member(&'a Path<'a>, &'a str),
}
impl Path<'_> {
    /// `$[0].name`, or `$["a key"]` for a member that is not an identifier.
    fn spell(&self, into: &mut String) -> Result<(), NativeError> {
        match self {
            Path::Root(root) => into.push_str(root),
            Path::Index(parent, index) => {
                parent.spell(into)?;
                into.push('[');
                into.push_str(&index.to_string());
                into.push(']');
            }
            Path::Member(parent, key) => {
                parent.spell(into)?;
                let identifier = !key.is_empty()
                    && key.chars().enumerate().all(|(index, c)| {
                        c == '_'
                            || c == '$'
                            || c.is_ascii_alphabetic()
                            || (index > 0 && c.is_ascii_digit())
                    });
                if identifier {
                    into.push('.');
                    into.push_str(key);
                } else {
                    // Keys use the same escaping routine as values.
                    let mut out = Out {
                        output: StringOutput(std::mem::take(into)),
                        args: None,
                        layout: None,
                        verbatim_kinds: 0,
                        verbatim_field: None,
                        encoding: None,
                        indentation: None,
                    };
                    out.output.0.push('[');
                    out.quote(key)?;
                    out.output.0.push(']');
                    *into = out.output.0;
                }
            }
        }
        Ok(())
    }
}

fn at(error: NativeError, context: &str, path: Path<'_>) -> NativeError {
    match error {
        NativeError::Refused { kind, message } => {
            let mut spelled = String::new();
            if let Err(error) = path.spell(&mut spelled) {
                return error;
            }
            NativeError::Refused {
                kind,
                message: format!("{context} {spelled}: {message}"),
            }
        }
        other => other,
    }
}
fn error(
    kind: &'static str,
    context: &str,
    path: Path<'_>,
    message: impl AsRef<str>,
) -> NativeError {
    at(raise(kind, message.as_ref()), context, path)
}
fn text<'k, A: Source<'k>>(
    source: &A,
    value: &A::Value,
    output: &mut impl Output,
) -> Result<Option<Str<'k>>, NativeError> {
    Ok(match source.view(value, output)? {
        View::Text(text) => Some(text),
        _ => None,
    })
}
fn omitted<V>(view: &View<'_, V>, target: Option<JsonTarget>) -> bool {
    target.is_some() && matches!(view, View::Absent)
        || target == Some(JsonTarget::Text) && matches!(view, View::Function)
}

fn normalize<'k, A: Source<'k>>(
    source: &A,
    mut view: View<'k, A::Value>,
    out: &mut Out<'_, '_, impl Output>,
    policy: &Policy<'_>,
) -> Result<View<'k, A::Value>, NativeError> {
    // Without a target only a mapping changes.
    if policy.target.is_none() && !matches!(view, View::Mapping(_)) {
        return Ok(view);
    }
    if let View::Mapping(fields) = view {
        let mut named = Vec::with_capacity(fields.len());
        for (key, value) in fields {
            let view = source.view(&key, &mut out.output)?;
            let name = match view {
                View::Text(text) => text.into_cow(),
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
                        legacy_markers: policy.legacy_markers,
                        active: Vec::new(),
                    }
                    .walk(key, view, 0, true, policy.path)?;
                    let rendered = key_out.output.0;
                    out.output.reserve(rendered.len() as u64)?;
                    Cow::Owned(rendered)
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
                        Some("String.prototype") => View::Text(Str::Held(Cow::Borrowed(""))),
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
        // A guest reads a kernel Int as a Number, so guest JSON text spells
        // it through binary64. Copied-out data sent to a host (a tool's
        // arguments, `encode_datum`) keeps the exact kernel value: an Int
        // crosses as its digits and is never rounded.
        let host_transport = policy.json_number && policy.target == Some(JsonTarget::Host);
        if !host_transport && let View::Number(Numeric::Int(integer)) = &view {
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
struct Encoder<'a, 'out, 'args, 'call, 'k, A: Source<'k>, S: Output> {
    source: &'a A,
    out: &'out mut Out<'args, 'call, S>,
    target: Option<JsonTarget>,
    context: &'a str,
    limit: usize,
    json_number: bool,
    legacy_markers: bool,
    active: Vec<A::Identity>,
}

pub(super) struct Policy<'a> {
    pub target: Option<JsonTarget>,
    pub context: &'a str,
    pub path: Path<'a>,
    pub limit: usize,
    /// Spell finite floats as [`Float::json_number`] does: the host's one
    /// number spelling, for copied-out data ([`encode_datum`]). Guest JSON
    /// text spells them by the caller's decimal layout instead.
    pub json_number: bool,
    pub legacy_markers: bool,
}

pub(super) fn encode<'k, A: Source<'k>>(
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
        legacy_markers: policy.legacy_markers,
        active: Vec::new(),
    }
    .walk(value, view, depth, raw_numbers, policy.path)
}

impl<'k, A: Source<'k>, S: Output> Encoder<'_, '_, '_, '_, 'k, A, S> {
    /// A scalar's spelling, at a depth the caller checked: what `walk`
    /// writes for a view with no members. A container's member loop writes
    /// its scalar members here directly, without a `walk` frame each.
    fn leaf(
        &mut self,
        view: View<'k, A::Value>,
        raw_numbers: bool,
        path: Path<'_>,
    ) -> Result<(), NativeError> {
        let source = self.source;
        let target = self.target;
        let context = self.context;
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
            _ => return Err(NativeError::Malformed("a JSON leaf holds members")),
        }
        Ok(())
    }

    /// The member at `depth` spelled by [`Self::leaf`]: a scalar within
    /// the nesting limit, which `walk` would neither omit nor descend.
    fn leaf_at(view: &View<'k, A::Value>, depth: usize, limit: usize) -> bool {
        depth <= limit
            && matches!(
                view,
                View::Null | View::Bool(_) | View::Number(_) | View::Text(_)
            )
    }

    fn walk(
        &mut self,
        value: A::Value,
        view: View<'k, A::Value>,
        depth: usize,
        raw_numbers: bool,
        path: Path<'_>,
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
            scalar @ (View::Null | View::Bool(_) | View::Number(_) | View::Text(_)) => {
                self.leaf(scalar, raw_numbers, path)?;
            }
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
                    let child = Path::Index(&path, index);
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
                                    path: child,
                                    limit,
                                    json_number: self.json_number,
                                    legacy_markers: self.legacy_markers,
                                },
                            )
                        })
                        .map_err(|e| at(e, context, child))?;
                    if Self::leaf_at(&view, depth + 1, limit) {
                        self.leaf(view, raw, child)?;
                    } else if !self.walk(item, view, depth + 1, raw, child)? {
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
                let branded = if target.is_none() {
                    false
                } else if self.legacy_markers {
                    fields.iter().any(|(key, _)| key == "__lash_brand")
                } else if let Some((_, brand)) =
                    fields.iter().find(|(key, _)| key == "__lash_brand")
                    && let View::Sequence(parts, "tuple") =
                        source.view(brand, &mut self.out.output)?
                    && parts.len() == 2
                {
                    text(source, &parts[0], &mut self.out.output)?.is_some()
                        && text(source, &parts[1], &mut self.out.output)?.is_some()
                } else {
                    false
                };
                if target.is_some()
                    && source.javascript()
                    && kind == "record"
                    && fields.iter().any(|(key, _)| array_index(key).is_some())
                {
                    fields.sort_by_cached_key(|(key, _)| {
                        array_index(key).map_or((1, 0), |index| (0, index))
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
                    if branded && matches!(key.as_ref(), "__lash_brand" | "__lash_built") {
                        continue;
                    }
                    let child = Path::Member(&path, &key);
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
                                    path: child,
                                    limit,
                                    json_number: self.json_number,
                                    legacy_markers: self.legacy_markers,
                                },
                            )
                        })
                        .map_err(|e| at(e, context, child))?;
                    if omitted(&view, target) {
                        continue;
                    }
                    if self.out.encoding.as_ref().is_some_and(|policy| {
                        kind == "record" && policy.excluded.iter().any(|field| field == &key)
                    }) {
                        return Err(error(
                            "not_data",
                            context,
                            child,
                            "record has an excluded field",
                        ));
                    }
                    if written > 0 {
                        self.out.separator(true)?;
                    }
                    self.out.newline(depth + 1)?;
                    if target == Some(JsonTarget::Host) {
                        self.out
                            .quote(&lash_kernel_doc::escape_function_key(&key))?;
                    } else {
                        self.out.quote(&key)?;
                    }
                    self.out.separator(false)?;
                    if self.out.indentation.is_some() && self.out.encoding.is_none() {
                        self.out.push(" ")?;
                    }
                    if Self::leaf_at(&view, depth + 1, limit) {
                        self.leaf(view, raw, child)?;
                    } else {
                        self.walk(item, view, depth + 1, raw, child)?;
                    }
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

/// `key` as a JavaScript array index: the canonical decimal spelling of a
/// whole number below 2^32 - 1, which an object lists first, in ascending
/// order. No sign, no leading zero but `0` itself.
fn array_index(key: &str) -> Option<u32> {
    let bytes = key.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 10
        || (bytes.len() > 1 && bytes[0] == b'0')
        || !bytes.iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    let index = bytes
        .iter()
        .fold(0u64, |value, digit| value * 10 + u64::from(digit - b'0'));
    u32::try_from(index).ok().filter(|index| *index < u32::MAX)
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
/// a JSON integer, and `2**53` stays `9007199254740992.0`. At the host target
/// a kernel Int is its exact digits under either model; a TypeScript BigInt
/// reaches here as that Int. No guest value is mutated.
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
            path: Path::Root("$"),
            limit,
            json_number: true,
            legacy_markers: false,
        },
    )?;
    if written {
        Ok(out.output.0)
    } else {
        Err(error(
            "not_data",
            context,
            Path::Root("$"),
            "undefined has no root JSON representation",
        ))
    }
}
