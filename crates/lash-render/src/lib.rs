//! Synchronous rendering of values without imposing a transport encoding on them.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ops::Range;

use serde::{Deserialize, Serialize};

pub enum RenderNode<'a> {
    Null,
    Undefined,
    Bool(bool),
    Number(Cow<'a, str>),
    Text(Cow<'a, str>),
    Array(usize),
    Object(usize),
    Placeholder(Cow<'a, str>),
}

pub trait RenderValue: Clone {
    fn node(&self) -> RenderNode<'_>;
    fn index(&self, i: usize) -> Option<Cow<'_, Self>>;
    fn fields(&self) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, Self>)> + '_;

    /// Error-like values render their stack instead of their other fields.
    fn stack(&self) -> Option<Cow<'_, str>> {
        self.fields().find_map(|(key, value)| {
            if key == "stack" {
                match value.node() {
                    RenderNode::Text(stack) => Some(Cow::Owned(stack.into_owned())),
                    _ => None,
                }
            } else {
                None
            }
        })
    }
}

impl RenderValue for serde_json::Value {
    fn node(&self) -> RenderNode<'_> {
        match self {
            Self::Null => RenderNode::Null,
            Self::Bool(value) => RenderNode::Bool(*value),
            Self::Number(value) => RenderNode::Number(Cow::Owned(value.to_string())),
            Self::String(value) => RenderNode::Text(Cow::Borrowed(value)),
            Self::Array(value) => RenderNode::Array(value.len()),
            Self::Object(value) => RenderNode::Object(value.len()),
        }
    }

    fn index(&self, i: usize) -> Option<Cow<'_, Self>> {
        self.as_array()?.get(i).map(Cow::Borrowed)
    }

    fn fields(&self) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, Self>)> + '_ {
        self.as_object()
            .into_iter()
            .flat_map(|map| map.iter())
            .map(|(key, value)| (Cow::Borrowed(key.as_str()), Cow::Borrowed(value)))
    }

    fn stack(&self) -> Option<Cow<'_, str>> {
        self.get("stack")?.as_str().map(Cow::Borrowed)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    Compact,
    Pretty,
    #[default]
    Auto,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderParams {
    pub max_chars: usize,
    pub layout: Layout,
    pub line_width: usize,
    pub indent: usize,
    pub max_depth: usize,
    pub array_threshold: usize,
    pub array_head: usize,
    pub array_tail: usize,
    pub min_item_chars: usize,
    pub stack_head: usize,
    pub stack_tail: usize,
}

impl Default for RenderParams {
    fn default() -> Self {
        Self {
            max_chars: 8_000,
            layout: Layout::Auto,
            line_width: 80,
            indent: 2,
            max_depth: 3,
            array_threshold: 10,
            array_head: 3,
            array_tail: 2,
            min_item_chars: 80,
            stack_head: 3,
            stack_tail: 1,
        }
    }
}

impl RenderParams {
    pub fn ax(max_chars: usize) -> Self {
        Self {
            max_chars,
            layout: Layout::Pretty,
            ..Self::default()
        }
    }

    pub fn preview() -> Self {
        Self {
            max_chars: 1_000,
            max_depth: 2,
            layout: Layout::Compact,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderParamsPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<Layout>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_width: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indent: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub array_threshold: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub array_head: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub array_tail: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_item_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack_head: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack_tail: Option<usize>,
}

impl RenderParamsPatch {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    pub fn over(&self, under: &Self) -> Self {
        macro_rules! choose {
            ($field:ident) => {
                self.$field.or(under.$field)
            };
        }
        Self {
            max_chars: choose!(max_chars),
            layout: choose!(layout),
            line_width: choose!(line_width),
            indent: choose!(indent),
            max_depth: choose!(max_depth),
            array_threshold: choose!(array_threshold),
            array_head: choose!(array_head),
            array_tail: choose!(array_tail),
            min_item_chars: choose!(min_item_chars),
            stack_head: choose!(stack_head),
            stack_tail: choose!(stack_tail),
        }
    }

    pub fn apply(&self, base: &RenderParams) -> RenderParams {
        macro_rules! choose {
            ($field:ident) => {
                self.$field.unwrap_or(base.$field)
            };
        }
        RenderParams {
            max_chars: choose!(max_chars),
            layout: choose!(layout),
            line_width: choose!(line_width),
            indent: choose!(indent),
            max_depth: choose!(max_depth),
            array_threshold: choose!(array_threshold),
            array_head: choose!(array_head),
            array_tail: choose!(array_tail),
            min_item_chars: choose!(min_item_chars),
            stack_head: choose!(stack_head),
            stack_tail: choose!(stack_tail),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CutKind {
    Array,
    Depth,
    Item,
    Stack,
    Chars,
    Lines,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShownRange {
    Text {
        block: usize,
        chars: Range<usize>,
        lines: Range<usize>,
    },
    ValuePath(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CutReport {
    pub original_chars: usize,
    pub counts: BTreeMap<CutKind, usize>,
    pub shown: Vec<ShownRange>,
}

impl CutReport {
    pub fn is_empty(&self) -> bool {
        self.counts.values().all(|count| *count == 0)
    }

    pub fn merge(&mut self, other: Self) {
        self.original_chars += other.original_chars;
        for (kind, count) in other.counts {
            *self.counts.entry(kind).or_default() += count;
        }
        self.shown.extend(other.shown);
    }

    fn add(&mut self, kind: CutKind) {
        *self.counts.entry(kind).or_default() += 1;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered<T> {
    pub body: T,
    pub cuts: CutReport,
}

struct Sink {
    body: Option<String>,
    chars: usize,
    column: usize,
}

impl Sink {
    fn new(collect: bool) -> Self {
        Self {
            body: collect.then(String::new),
            chars: 0,
            column: 0,
        }
    }

    fn write(&mut self, text: &str) {
        self.chars += text.chars().count();
        self.column = text.rsplit('\n').next().map_or(0, |tail| {
            if text.contains('\n') {
                tail.chars().count()
            } else {
                self.column + tail.chars().count()
            }
        });
        if let Some(body) = &mut self.body {
            body.push_str(text);
        }
    }

    fn spaces(&mut self, count: usize) {
        for _ in 0..count {
            self.write(" ");
        }
    }

    fn finish(self) -> String {
        self.body.unwrap_or_default()
    }
}

#[derive(Clone, Copy)]
enum WalkLayout {
    Compact,
    Pretty,
    Auto,
    Inline,
}

#[derive(Clone, Copy)]
struct WalkConfig {
    depth: usize,
    layout: WalkLayout,
    limited: bool,
    root_text_raw: bool,
}

impl WalkConfig {
    fn child(self) -> Self {
        Self {
            depth: self.depth + 1,
            root_text_raw: false,
            ..self
        }
    }
}

fn write_json_string(sink: &mut Sink, value: &str) {
    sink.write("\"");
    let mut segment = 0;
    for (index, ch) in value.char_indices() {
        let escape = match ch {
            '"' => Some("\\\""),
            '\\' => Some("\\\\"),
            '\u{0008}' => Some("\\b"),
            '\t' => Some("\\t"),
            '\n' => Some("\\n"),
            '\u{000c}' => Some("\\f"),
            '\r' => Some("\\r"),
            _ => None,
        };
        if let Some(escape) = escape {
            sink.write(&value[segment..index]);
            sink.write(escape);
            segment = index + ch.len_utf8();
        } else if ch <= '\u{001f}' {
            sink.write(&value[segment..index]);
            const HEX: [&str; 16] = [
                "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "a", "b", "c", "d", "e", "f",
            ];
            let code = ch as usize;
            sink.write("\\u00");
            sink.write(HEX[code >> 4]);
            sink.write(HEX[code & 15]);
            segment = index + ch.len_utf8();
        }
    }
    sink.write(&value[segment..]);
    sink.write("\"");
}

fn walk_layout(params: &RenderParams) -> WalkLayout {
    match params.layout {
        Layout::Compact => WalkLayout::Compact,
        Layout::Pretty if params.indent == 0 => WalkLayout::Compact,
        Layout::Pretty => WalkLayout::Pretty,
        Layout::Auto => WalkLayout::Auto,
    }
}

fn write_value<V: RenderValue>(
    value: &V,
    params: &RenderParams,
    sink: &mut Sink,
    cuts: &mut CutReport,
    config: WalkConfig,
) {
    let node = value.node();
    match node {
        RenderNode::Null => sink.write("null"),
        RenderNode::Undefined => sink.write("undefined"),
        RenderNode::Bool(value) => sink.write(if value { "true" } else { "false" }),
        RenderNode::Number(value) | RenderNode::Placeholder(value) => sink.write(&value),
        RenderNode::Text(value) if config.root_text_raw => sink.write(&value),
        RenderNode::Text(value) => write_json_string(sink, &value),
        RenderNode::Array(len) | RenderNode::Object(len) => {
            let array = matches!(node, RenderNode::Array(_));
            if config.limited && config.depth >= params.max_depth {
                let marker = if array {
                    format!("[Array({len})]")
                } else {
                    "[Object]".to_owned()
                };
                write_json_string(sink, &marker);
                cuts.add(CutKind::Depth);
                return;
            }

            let layout = if matches!(config.layout, WalkLayout::Auto) {
                let mut candidate = Sink::new(false);
                write_container(
                    value,
                    params,
                    &mut candidate,
                    &mut CutReport::default(),
                    WalkConfig {
                        layout: WalkLayout::Inline,
                        ..config
                    },
                    array,
                    len,
                );
                if sink.column.saturating_add(candidate.chars) <= params.line_width {
                    WalkLayout::Inline
                } else {
                    WalkLayout::Auto
                }
            } else {
                config.layout
            };
            write_container(
                value,
                params,
                sink,
                cuts,
                WalkConfig { layout, ..config },
                array,
                len,
            );
        }
    }
}

fn write_container<V: RenderValue>(
    value: &V,
    params: &RenderParams,
    sink: &mut Sink,
    cuts: &mut CutReport,
    config: WalkConfig,
    array: bool,
    len: usize,
) {
    sink.write(if array { "[" } else { "{" });
    let mut count = 0;
    if array {
        for index in 0..len {
            write_entry_prefix(sink, params, config, count);
            match value.index(index) {
                None => sink.write("null"),
                Some(child) if matches!(child.node(), RenderNode::Undefined) => sink.write("null"),
                Some(child) => write_value(child.as_ref(), params, sink, cuts, config.child()),
            }
            count += 1;
        }
    } else {
        for (key, child) in value.fields() {
            if matches!(child.node(), RenderNode::Undefined) {
                continue;
            }
            write_entry_prefix(sink, params, config, count);
            write_json_string(sink, &key);
            sink.write(if matches!(config.layout, WalkLayout::Compact) {
                ":"
            } else {
                ": "
            });
            write_value(child.as_ref(), params, sink, cuts, config.child());
            count += 1;
        }
    }
    if matches!(config.layout, WalkLayout::Pretty | WalkLayout::Auto) && count > 0 {
        sink.write("\n");
        sink.spaces(config.depth.saturating_mul(params.indent));
    }
    sink.write(if array { "]" } else { "}" });
}

fn write_entry_prefix(sink: &mut Sink, params: &RenderParams, config: WalkConfig, index: usize) {
    if index > 0 {
        sink.write(",");
    }
    if matches!(config.layout, WalkLayout::Pretty | WalkLayout::Auto) {
        sink.write("\n");
        sink.spaces((config.depth + 1).saturating_mul(params.indent));
    } else if index > 0 && matches!(config.layout, WalkLayout::Inline) {
        sink.write(" ");
    }
}

fn compress_stack(stack: &str, params: &RenderParams, cuts: &mut CutReport) -> String {
    let lines: Vec<_> = stack.split('\n').collect();
    let Some(start) = lines.iter().position(|line| {
        let trimmed = line.trim_start_matches(char::is_whitespace);
        trimmed.len() != line.len()
            && trimmed
                .strip_prefix("at")
                .and_then(|rest| rest.chars().next())
                .is_some_and(char::is_whitespace)
    }) else {
        return stack.to_owned();
    };
    let frames = &lines[start..];
    let keep = params.stack_head.saturating_add(params.stack_tail);
    if frames.len() <= keep {
        return stack.to_owned();
    }
    let hidden = frames.len() - keep;
    let mut result = lines[..start].to_vec();
    result.extend(frames.iter().take(params.stack_head).copied());
    let marker = format!("    ... [{hidden} frames hidden]");
    result.push(&marker);
    result.extend(
        frames
            .iter()
            .skip(frames.len() - params.stack_tail)
            .copied(),
    );
    cuts.add(CutKind::Stack);
    result.join("\n")
}

fn sample_array<V: RenderValue>(
    value: &V,
    len: usize,
    params: &RenderParams,
    cuts: &mut CutReport,
) -> String {
    let head = params.array_head.min(len);
    let tail = params.array_tail.min(len - head);
    let budget = params.min_item_chars.max(
        params.max_chars
            / params
                .array_head
                .saturating_add(params.array_tail)
                .saturating_add(1),
    );
    let mut items = Vec::new();
    for index in (0..head).chain(len - tail..len) {
        let mut sink = Sink::new(true);
        if let Some(child) = value.index(index) {
            write_value(
                child.as_ref(),
                params,
                &mut sink,
                &mut CutReport::default(),
                WalkConfig {
                    depth: 0,
                    layout: WalkLayout::Compact,
                    limited: false,
                    root_text_raw: false,
                },
            );
        } else {
            sink.write("null");
        }
        let item = sink.finish();
        let length = item.chars().count();
        let item = if length > budget {
            cuts.add(CutKind::Item);
            let prefix: String = item.chars().take(budget.saturating_sub(3)).collect();
            format!("{prefix}...")
        } else {
            item
        };
        cuts.shown.push(ShownRange::ValuePath(format!("[{index}]")));
        items.push(item);
    }
    let hidden = len - head - tail;
    cuts.add(CutKind::Array);
    let marker = format!("... [{hidden} hidden items]");
    let (first, last) = items.split_at(head);
    let mut framed = Vec::with_capacity(items.len() + 1);
    framed.extend(first.iter().map(String::as_str));
    framed.push(&marker);
    framed.extend(last.iter().map(String::as_str));
    match params.layout {
        Layout::Compact => format!("[{}]", framed.join(",")),
        Layout::Pretty | Layout::Auto => {
            let pad = " ".repeat(params.indent);
            format!("[\n{pad}{}\n]", framed.join(&format!(",\n{pad}")))
        }
    }
}

pub fn render<V: RenderValue>(value: &V, params: &RenderParams) -> Rendered<String> {
    let mut full = Sink::new(false);
    let mut ignored = CutReport::default();
    if let Some(stack) = value.stack() {
        full.write(&stack);
    } else {
        let layout = walk_layout(params);
        write_value(
            value,
            params,
            &mut full,
            &mut ignored,
            WalkConfig {
                depth: 0,
                layout,
                limited: false,
                root_text_raw: true,
            },
        );
    }
    let mut cuts = CutReport {
        original_chars: full.chars,
        ..CutReport::default()
    };
    let body = if let Some(stack) = value.stack() {
        compress_stack(&stack, params, &mut cuts)
    } else if let RenderNode::Array(len) = value.node() {
        if len > params.array_threshold {
            sample_array(value, len, params, &mut cuts)
        } else {
            render_limited(value, params, &mut cuts)
        }
    } else {
        render_limited(value, params, &mut cuts)
    };
    Rendered { body, cuts }
}

fn render_limited<V: RenderValue>(
    value: &V,
    params: &RenderParams,
    cuts: &mut CutReport,
) -> String {
    let mut sink = Sink::new(true);
    let layout = walk_layout(params);
    write_value(
        value,
        params,
        &mut sink,
        cuts,
        WalkConfig {
            depth: 0,
            layout,
            limited: true,
            root_text_raw: true,
        },
    );
    sink.finish()
}

pub fn truncate_chars(mut rendered: Rendered<String>, max_chars: usize) -> Rendered<String> {
    let chars = rendered.body.chars().count();
    if chars > max_chars {
        let prefix: String = rendered.body.chars().take(max_chars).collect();
        let shown_lines = if prefix.is_empty() {
            0
        } else {
            prefix.split('\n').count()
        };
        rendered.cuts.add(CutKind::Chars);
        rendered.cuts.shown.push(ShownRange::Text {
            block: 0,
            chars: 0..max_chars,
            lines: 0..shown_lines,
        });
        rendered.body = format!("{prefix}\n...[truncated {} chars]", chars - max_chars);
    }
    rendered
}
