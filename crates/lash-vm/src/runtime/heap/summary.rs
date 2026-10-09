// A bounded, human-readable summary of a runtime value, for a prompt.
//
// The host view carries a binding only when it has a detached host shape, so a
// `Map`, a `Date` or a record holding one never appears there (ADR 0076). A
// model still has to know such a binding exists and roughly what it holds, so
// this renders any runtime value — exotics included — the way a ECMA-262
// console would show it, cut to a fixed number of members, a fixed depth and a
// fixed length. It is a description, never a value: nothing reads it back.

use super::{Heap, HeapObject, Value, regexp_string, serialize_params};

/// Presentation of opaque heap bindings. These historical cuts have no
/// workload measurement establishing them as universal limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BindingSummaryConfig {
    pub members: usize,
    pub depth: usize,
    pub max_chars: usize,
}
impl Default for BindingSummaryConfig {
    fn default() -> Self {
        Self::standard()
    }
}
impl BindingSummaryConfig {
    /// Standard preset: four members, two container levels, 160 characters.
    pub const fn standard() -> Self {
        Self {
            members: 4,
            depth: 2,
            max_chars: 160,
        }
    }
}
pub(crate) const SUMMARY_MAX_CHARS: usize = BindingSummaryConfig::standard().max_chars;

struct SummaryView<'a> {
    heap: &'a Heap,
    config: &'a BindingSummaryConfig,
}
impl Heap {
    pub(crate) fn summarize(&self, value: &Value, config: &BindingSummaryConfig) -> String {
        SummaryView { heap: self, config }.summarize(value)
    }
}

impl SummaryView<'_> {
    /// `value`, summarized within the bounds above.
    pub(crate) fn summarize(&self, value: &Value) -> String {
        let mut text = String::new();
        self.summarize_into(value, 0, &mut text);
        if text.chars().count() > self.config.max_chars {
            let mut cut = text
                .chars()
                .take(self.config.max_chars.saturating_sub(1))
                .collect::<String>();
            if self.config.max_chars > 0 {
                cut.push('…');
            }
            return cut;
        }
        text
    }

    fn summarize_into(&self, value: &Value, depth: usize, out: &mut String) {
        match value {
            Value::Null => out.push_str("null"),
            Value::Undefined => out.push_str("undefined"),
            Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
            Value::Number(number) => out.push_str(&summarize_number(*number)),
            Value::String(text) => out.push_str(&quote(text)),
            Value::Image(image) => out.push_str(&format!("<image {}>", image.label)),
            Value::Resource(handle) => {
                out.push_str(&format!("<{} {}>", handle.resource_type, handle.alias));
            }
            Value::Projected(projected) => {
                out.push_str(&format!(
                    "<{} {}>",
                    projected.value_type_name(),
                    projected.name()
                ));
            }
            Value::Tuple(items) | Value::List(items) => {
                self.summarize_members(
                    depth,
                    "[",
                    "]",
                    items.len(),
                    items.iter(),
                    out,
                    |heap, item, depth, out| heap.summarize_into(item, depth, out),
                );
            }
            Value::Record(record) => self.summarize_record(record.iter(), record.len(), depth, out),
            Value::Ref(id) => match self.heap.get(*id) {
                Ok(object) => self.summarize_object(*id, object, depth, out),
                Err(_) => out.push_str("<unavailable>"),
            },
        }
    }

    fn summarize_object(
        &self,
        id: super::HeapId,
        object: &HeapObject,
        depth: usize,
        out: &mut String,
    ) {
        match object {
            HeapObject::Tuple(items) | HeapObject::List { items, .. } => {
                self.summarize_members(
                    depth,
                    "[",
                    "]",
                    items.len(),
                    items.iter(),
                    out,
                    |heap, item, depth, out| heap.summarize_into(item, depth, out),
                );
            }
            HeapObject::Record(record) => {
                self.summarize_record(record.iter(), record.len(), depth, out)
            }
            HeapObject::Map(map) => {
                out.push_str(&format!("Map({}) ", map.entries.len()));
                self.summarize_members(
                    depth,
                    "{",
                    "}",
                    map.entries.len(),
                    map.entries.iter(),
                    out,
                    |heap, (key, value), depth, out| {
                        heap.summarize_into(key, depth, out);
                        out.push_str(" => ");
                        heap.summarize_into(value, depth, out);
                    },
                );
            }
            HeapObject::Set(set) => {
                out.push_str(&format!("Set({}) ", set.values.len()));
                self.summarize_members(
                    depth,
                    "{",
                    "}",
                    set.values.len(),
                    set.values.iter(),
                    out,
                    |heap, item, depth, out| heap.summarize_into(item, depth, out),
                );
            }
            HeapObject::Date(date) => {
                let text = crate::runtime::vm::javascript_date::to_iso_string(date.milliseconds)
                    .unwrap_or_else(|| "Invalid Date".to_string());
                out.push_str(&format!("Date({text})"));
            }
            HeapObject::RegExp(regexp) => {
                out.push_str(&regexp_string(regexp));
                if regexp.last_index != Value::Number(0.0) {
                    out.push_str(" (lastIndex ");
                    self.summarize_into(&regexp.last_index, depth + 1, out);
                    out.push(')');
                }
            }
            HeapObject::RegExpMatch(result) => {
                out.push_str("RegExp match ");
                self.summarize_members(
                    depth,
                    "[",
                    "]",
                    result.items.len(),
                    result.items.iter(),
                    out,
                    |heap, item, depth, out| heap.summarize_into(item, depth, out),
                );
                out.push_str(" at ");
                self.summarize_into(&result.index, depth + 1, out);
            }
            HeapObject::Url(_) => match self.heap.url_property(id, "href") {
                Ok(Some(Value::String(href))) => out.push_str(&format!("URL({})", quote(&href))),
                _ => out.push_str("URL"),
            },
            HeapObject::UrlSearchParams(params) => {
                out.push_str(&format!(
                    "URLSearchParams({})",
                    quote(&serialize_params(&params.entries))
                ));
            }
            HeapObject::Error(error) => {
                out.push_str(error.kind.name());
                if let Some(message) = error.message.as_deref().filter(|m| !m.is_empty()) {
                    out.push_str(": ");
                    out.push_str(message);
                }
            }
            HeapObject::Closure { .. } => out.push_str("function"),
            // Never a guest value; a summary only meets one through a frame
            // slot, where it stands for the binding's current value.
            HeapObject::Cell(value) => self.summarize_into(value, depth, out),
            HeapObject::BuiltinFunction(function) => {
                out.push_str("function ");
                out.push_str(function.name());
            }
        }
    }

    fn summarize_record<'a>(
        &self,
        fields: impl Iterator<Item = (&'a str, &'a Value)>,
        len: usize,
        depth: usize,
        out: &mut String,
    ) {
        self.summarize_members(
            depth,
            "{ ",
            " }",
            len,
            fields,
            out,
            |heap, (name, value), depth, out| {
                out.push_str(name);
                out.push_str(": ");
                heap.summarize_into(value, depth, out);
            },
        );
    }

    /// `open` members… `close`, at most the configured member count; below
    /// the configured depth a non-empty container collapses to `open…close`.
    #[expect(
        clippy::too_many_arguments,
        reason = "one walker serves every container kind; its bounds and callbacks are its arguments"
    )]
    fn summarize_members<T>(
        &self,
        depth: usize,
        open: &str,
        close: &str,
        len: usize,
        members: impl Iterator<Item = T>,
        out: &mut String,
        member: impl Fn(&Self, T, usize, &mut String),
    ) {
        if len == 0 {
            out.push_str(open.trim_end());
            out.push_str(close.trim_start());
            return;
        }
        out.push_str(open);
        if depth >= self.config.depth {
            out.push('…');
        } else {
            for (index, item) in members.take(self.config.members).enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                member(self, item, depth + 1, out);
            }
            if len > self.config.members {
                out.push_str(&format!(", … {} more", len - self.config.members));
            }
        }
        out.push_str(close);
    }
}

fn quote(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| format!("\"{text}\""))
}

fn summarize_number(number: f64) -> String {
    if number.is_nan() {
        "NaN".to_string()
    } else if number.is_infinite() {
        if number > 0.0 {
            "Infinity"
        } else {
            "-Infinity"
        }
        .to_string()
    } else if number == number.trunc() && number.abs() < 1e21 {
        format!("{number:.0}")
    } else {
        number.to_string()
    }
}
