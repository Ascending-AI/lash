// A bounded, human-readable summary of a runtime value, for a prompt.
//
// The host view carries a binding only when it has a detached host shape, so a
// `Map`, a `Date` or a record holding one never appears there (ADR 0076). A
// model still has to know such a binding exists and roughly what it holds, so
// this renders any runtime value — exotics included — the way a JavaScript
// console would show it, cut to a fixed number of members, a fixed depth and a
// fixed length. It is a description, never a value: nothing reads it back.

use super::{Heap, HeapObject, Value, regexp_string, serialize_params};

/// Members shown per container before an ellipsis.
const SUMMARY_MEMBERS: usize = 4;
/// Nesting shown before a container collapses to its kind.
const SUMMARY_DEPTH: usize = 2;
/// The longest summary, in characters.
pub(crate) const SUMMARY_MAX_CHARS: usize = 160;

impl Heap {
    /// `value`, summarized within the bounds above.
    pub(crate) fn summarize(&self, value: &Value) -> String {
        let mut text = String::new();
        self.summarize_into(value, 0, &mut text);
        if text.chars().count() > SUMMARY_MAX_CHARS {
            let mut cut = text.chars().take(SUMMARY_MAX_CHARS - 1).collect::<String>();
            cut.push('…');
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
            Value::Ref(id) => match self.get(*id) {
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
            HeapObject::Tuple(items) | HeapObject::List(items) => {
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
                if regexp.last_index != 0 {
                    out.push_str(&format!(" (lastIndex {})", regexp.last_index));
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
            HeapObject::Url(_) => match self.url_property(id, "href") {
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

    /// `open` members… `close`, at most `SUMMARY_MEMBERS` of them; below
    /// `SUMMARY_DEPTH` a non-empty container collapses to `open…close`.
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
        if depth >= SUMMARY_DEPTH {
            out.push('…');
        } else {
            for (index, item) in members.take(SUMMARY_MEMBERS).enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                member(self, item, depth + 1, out);
            }
            if len > SUMMARY_MEMBERS {
                out.push_str(&format!(", … {} more", len - SUMMARY_MEMBERS));
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
