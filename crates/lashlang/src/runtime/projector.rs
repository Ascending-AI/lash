use std::fmt::Write as _;

use super::{ProjectedFuture, Value, write_number};

const DEFAULT_ARRAY_HEAD: usize = 8;
const DEFAULT_ARRAY_TAIL: usize = 2;
const DEFAULT_OBJECT_FIELDS: usize = 12;
const TRUNCATED_MARKER: &str = "...truncated...";

const DIAGNOSTIC_FIELDS: &[&str] = &[
    "status",
    "success",
    "ok",
    "error",
    "code",
    "exit_code",
    "stderr",
    "stdout",
    "message",
    "duration_ms",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BudgetedJsonProjectionConfig {
    pub max_bytes: usize,
    pub max_lines: usize,
    pub max_depth: usize,
}

impl BudgetedJsonProjectionConfig {
    pub const fn new(max_bytes: usize, max_lines: usize, max_depth: usize) -> Self {
        Self {
            max_bytes,
            max_lines,
            max_depth,
        }
    }

    pub const fn unbounded() -> Self {
        Self {
            max_bytes: usize::MAX,
            max_lines: usize::MAX,
            max_depth: usize::MAX,
        }
    }
}

impl Default for BudgetedJsonProjectionConfig {
    fn default() -> Self {
        Self::new(16 * 1024, 400, 6)
    }
}

#[derive(Clone, Copy)]
pub struct ValueProjectionContext<'a> {
    pub value: &'a Value,
}

impl<'a> ValueProjectionContext<'a> {
    pub fn new(value: &'a Value) -> Self {
        Self { value }
    }
}

pub trait ValueProjector: Send + Sync {
    fn project<'a>(&'a self, context: ValueProjectionContext<'a>) -> ProjectedFuture<'a, String>;
}

#[derive(Clone, Debug)]
pub struct BudgetedJsonProjector {
    config: BudgetedJsonProjectionConfig,
}

impl BudgetedJsonProjector {
    pub const fn new(config: BudgetedJsonProjectionConfig) -> Self {
        Self { config }
    }

    pub const fn config(&self) -> BudgetedJsonProjectionConfig {
        self.config
    }

    pub const fn unbounded() -> Self {
        Self::new(BudgetedJsonProjectionConfig::unbounded())
    }

    pub fn project_blocking(&self, context: ValueProjectionContext<'_>) -> String {
        futures_executor::block_on(self.project(context))
    }

    fn is_unbounded(&self) -> bool {
        self.config == BudgetedJsonProjectionConfig::unbounded()
    }

    fn render_value<'a>(
        &'a self,
        value: &'a Value,
        depth: usize,
        top_level: bool,
    ) -> ProjectedFuture<'a, String> {
        Box::pin(async move {
            match value {
                Value::Null => "null".to_string(),
                Value::Bool(value) => value.to_string(),
                Value::Number(value) => {
                    let mut out = String::new();
                    write_number(&mut out, *value).expect("string writes should not fail");
                    out
                }
                Value::String(value) if top_level => value.to_string(),
                Value::String(value) => json_string(&clip_nested_string(value, self.config)),
                Value::Image(image) => {
                    serde_json::to_string(image).unwrap_or_else(|_| "null".to_string())
                }
                Value::Resource(resource) => {
                    serde_json::to_string(resource).unwrap_or_else(|_| "null".to_string())
                }
                Value::Projected(projected) => projected.render().await,
                Value::Ref(_) => {
                    unreachable!("heap references must be exported before projection")
                }
                Value::Tuple(values) | Value::List(values) => {
                    if self.config.max_depth != usize::MAX && depth >= self.config.max_depth {
                        return depth_marker(self.config.max_depth);
                    }
                    let len = values.len();
                    if len == 0 {
                        return "[]".to_string();
                    }
                    let mut out = String::from("[");
                    let mut omitted = 0usize;
                    for (index, value) in values.iter().enumerate() {
                        if !self.is_unbounded()
                            && len > DEFAULT_ARRAY_HEAD + DEFAULT_ARRAY_TAIL
                            && index >= DEFAULT_ARRAY_HEAD
                            && index < len - DEFAULT_ARRAY_TAIL
                        {
                            omitted += 1;
                            continue;
                        }
                        if index > 0 && !out.ends_with('[') {
                            out.push(',');
                        }
                        if omitted > 0 {
                            let _ = write!(
                                out,
                                "{}",
                                json_string(&format!("... {omitted} items omitted ..."))
                            );
                            out.push(',');
                            omitted = 0;
                        }
                        out.push_str(&self.render_value(value, depth + 1, false).await);
                    }
                    if omitted > 0 {
                        if !out.ends_with('[') {
                            out.push(',');
                        }
                        let _ = write!(
                            out,
                            "{}",
                            json_string(&format!("... {omitted} items omitted ..."))
                        );
                    }
                    out.push(']');
                    out
                }
                Value::Record(record) => {
                    if self.config.max_depth != usize::MAX && depth >= self.config.max_depth {
                        return depth_marker(self.config.max_depth);
                    }
                    if record.is_empty() {
                        return "{}".to_string();
                    }
                    let mut entries: Vec<_> = record.iter().collect();
                    if !self.is_unbounded() {
                        entries = prioritized_record_entries(entries);
                    }
                    let total = entries.len();
                    let mut out = String::from("{");
                    let mut shown = 0usize;
                    for (key, value) in entries.drain(..) {
                        if !self.is_unbounded()
                            && shown >= DEFAULT_OBJECT_FIELDS
                            && !DIAGNOSTIC_FIELDS.contains(&key)
                        {
                            continue;
                        }
                        if shown > 0 {
                            out.push(',');
                        }
                        out.push_str(&json_string(key));
                        out.push(':');
                        out.push_str(&self.render_value(value, depth + 1, false).await);
                        shown += 1;
                    }
                    if shown < total {
                        if shown > 0 {
                            out.push(',');
                        }
                        out.push_str("\"__truncated__\":");
                        out.push_str(&json_string(&format!("{} fields omitted", total - shown)));
                    }
                    out.push('}');
                    out
                }
            }
        })
    }

    fn enforce_budget(&self, rendered: String) -> String {
        truncate_lines(
            truncate_bytes(rendered, self.config.max_bytes),
            self.config.max_lines,
        )
    }
}

impl ValueProjector for BudgetedJsonProjector {
    fn project<'a>(&'a self, context: ValueProjectionContext<'a>) -> ProjectedFuture<'a, String> {
        Box::pin(async move {
            let rendered = self.render_value(context.value, 0, true).await;
            self.enforce_budget(rendered)
        })
    }
}

fn prioritized_record_entries<'a>(
    mut entries: Vec<(&'a str, &'a Value)>,
) -> Vec<(&'a str, &'a Value)> {
    entries.sort_by_key(|(key, _)| {
        DIAGNOSTIC_FIELDS
            .iter()
            .position(|field| field == key)
            .unwrap_or(DIAGNOSTIC_FIELDS.len())
    });
    entries
}

fn depth_marker(max_depth: usize) -> String {
    json_string(&format!("... max depth {max_depth} reached ..."))
}

fn clip_nested_string(text: &str, config: BudgetedJsonProjectionConfig) -> String {
    if config == BudgetedJsonProjectionConfig::unbounded() {
        return text.to_string();
    }
    let max_chars = config.max_bytes.min(1024).saturating_div(4).max(64);
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(32).max(16);
    let head: String = text.chars().take(keep).collect();
    let omitted = text.chars().count().saturating_sub(keep);
    format!("{head}... {omitted} chars truncated ...")
}

fn truncate_lines(text: String, max_lines: usize) -> String {
    if max_lines == usize::MAX || max_lines == 0 {
        return text;
    }
    let mut lines = text.lines();
    let retained = lines.by_ref().take(max_lines).collect::<Vec<_>>();
    if lines.next().is_none() {
        return text;
    }
    format!("{}\n{TRUNCATED_MARKER}", retained.join("\n"))
}

fn truncate_bytes(mut text: String, max_bytes: usize) -> String {
    if max_bytes == usize::MAX || text.len() <= max_bytes {
        return text;
    }
    let cut = floor_char_boundary(&text, max_bytes);
    text.truncate(cut);
    if text.ends_with('\n') {
        text.push_str(TRUNCATED_MARKER);
    } else {
        text.push('\n');
        text.push_str(TRUNCATED_MARKER);
    }
    text
}

fn floor_char_boundary(text: &str, max: usize) -> usize {
    if max >= text.len() {
        return text.len();
    }
    let mut cut = max;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    cut
}

fn json_string(text: &str) -> String {
    serde_json::to_string(text).expect("string json serialization should succeed")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{ImageValue, ProjectedValue, Record, ResourceHandle};

    fn project(value: &Value, config: BudgetedJsonProjectionConfig) -> String {
        futures_executor::block_on(
            BudgetedJsonProjector::new(config).project(ValueProjectionContext::new(value)),
        )
    }

    fn default_project(value: &Value) -> String {
        project(value, BudgetedJsonProjectionConfig::new(4096, 200, 6))
    }

    #[test]
    fn projects_scalars_and_strings_exactly() {
        assert_eq!(default_project(&Value::Null), "null");
        assert_eq!(default_project(&Value::Bool(true)), "true");
        assert_eq!(default_project(&Value::Number(42.0)), "42");
        assert_eq!(default_project(&Value::String("hello".into())), "hello");
    }

    #[test]
    fn projects_lists_and_records_as_compact_json() {
        let list = Value::List(
            vec![
                Value::Number(1.0),
                Value::String("two".into()),
                Value::Bool(false),
            ]
            .into(),
        );
        assert_eq!(default_project(&list), r#"[1,"two",false]"#);

        let mut record = Record::new();
        record.insert_str("b", Value::Number(2.0));
        record.insert_str("a", Value::Number(1.0));
        assert_eq!(
            default_project(&Value::Record(Arc::new(record))),
            r#"{"b":2,"a":1}"#
        );
    }

    #[test]
    fn projects_image_and_resource_descriptors() {
        assert_eq!(
            default_project(&Value::Image(Box::new(ImageValue::new(
                "img-1",
                crate::MediaType::parse("image/png").unwrap(),
                "plot.png",
                123,
                Some(10),
                Some(20),
            )))),
            r#"{"type":"image","id":"img-1","mime":"image/png","label":"plot.png","size":123,"width":10,"height":20}"#
        );
        assert_eq!(
            default_project(&Value::Resource(ResourceHandle::new("files", "workspace"))),
            r#"{"resource_type":"files","alias":"workspace"}"#
        );
    }

    #[test]
    fn projects_projected_values_by_host_rendering() {
        let value = Value::Projected(ProjectedValue::scalar(
            "host",
            Value::Record(Arc::new({
                let mut record = Record::new();
                record.insert_str("answer", Value::Number(7.0));
                record
            })),
        ));

        assert_eq!(default_project(&value), r#"{"answer":7}"#);
    }

    #[test]
    fn caps_deep_nesting() {
        let mut c = Record::new();
        c.insert_str("c", Value::Number(1.0));
        let mut b = Record::new();
        b.insert_str("b", Value::Record(Arc::new(c)));
        let mut a = Record::new();
        a.insert_str("a", Value::Record(Arc::new(b)));

        assert_eq!(
            project(
                &Value::Record(Arc::new(a)),
                BudgetedJsonProjectionConfig::new(4096, 200, 2),
            ),
            r#"{"a":{"b":"... max depth 2 reached ..."}}"#
        );
    }

    #[test]
    fn caps_wide_objects() {
        let mut record = Record::new();
        for index in 0..15 {
            record.insert_str(&format!("k{index}"), Value::Number(index as f64));
        }

        assert_eq!(
            default_project(&Value::Record(Arc::new(record))),
            r#"{"k0":0,"k1":1,"k2":2,"k3":3,"k4":4,"k5":5,"k6":6,"k7":7,"k8":8,"k9":9,"k10":10,"k11":11,"__truncated__":"3 fields omitted"}"#
        );
    }

    #[test]
    fn caps_long_arrays_with_head_and_tail() {
        let values = (0..16)
            .map(|value| Value::Number(value as f64))
            .collect::<Vec<_>>();

        assert_eq!(
            default_project(&Value::List(values.into())),
            r#"[0,1,2,3,4,5,6,7,"... 6 items omitted ...",14,15]"#
        );
    }

    #[test]
    fn clips_utf8_at_char_boundary() {
        let projected = project(
            &Value::String("éééé".into()),
            BudgetedJsonProjectionConfig::new(7, 200, 6),
        );

        assert_eq!(projected, "ééé\n...truncated...");
        assert!(projected.is_char_boundary(projected.len()));
    }

    #[test]
    fn preserves_diagnostic_fields_before_large_payloads() {
        let mut record = Record::new();
        record.insert_str("output", Value::String("x".repeat(2000).into()));
        record.insert_str("status", Value::String("failed".into()));
        record.insert_str("error", Value::String("boom".into()));
        record.insert_str("exit_code", Value::Number(2.0));
        record.insert_str("stderr", Value::String("short stderr".into()));

        let projected = project(
            &Value::Record(Arc::new(record)),
            BudgetedJsonProjectionConfig::new(512, 200, 6),
        );

        assert!(projected.starts_with(
            r#"{"status":"failed","error":"boom","exit_code":2,"stderr":"short stderr","output":"#
        ));
        assert!(projected.contains("chars truncated"));
    }
}
