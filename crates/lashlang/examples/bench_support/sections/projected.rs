fn projected_docs_record() -> Value {
    let mut record = Record::default();
    record.insert("title".to_string(), Value::String("Authoring Rules".into()));
    record.insert(
        "summary".to_string(),
        Value::String("Rules for editing workflow JSON drafts.".into()),
    );
    record.insert(
        "body".to_string(),
        Value::Projected(ProjectedValue::custom(
            "docs.body",
            Arc::new(ProjectedText::new("body", "lazy markdown body")),
        )),
    );
    Value::Record(Arc::new(record))
}

fn snapshot_projected_record() -> Value {
    let mut projected = Record::default();
    projected.insert(
        "body".to_string(),
        Value::Projected(ProjectedValue::custom(
            "snap.projected.body",
            Arc::new(ProjectedText::new(
                "snapshot_body",
                "projected body stays lazy across snapshot markers",
            )),
        )),
    );

    let mut normal = Record::default();
    normal.insert("title".to_string(), Value::String("Snapshot Rules".into()));
    normal.insert(
        "tags".to_string(),
        Value::List(
            vec![
                Value::String("snapshot".into()),
                Value::String("projected".into()),
                Value::String("mixed".into()),
            ]
            .into(),
        ),
    );

    let mut nested = Record::default();
    nested.insert(
        "projected_title".to_string(),
        Value::Projected(ProjectedValue::custom(
            "snap.mixed.nested.projected_title",
            Arc::new(ProjectedText::new("nested_title", "Nested Projection")),
        )),
    );

    let mut mixed = Record::default();
    mixed.insert("count".to_string(), Value::Number(7.0));
    mixed.insert("nested".to_string(), Value::Record(Arc::new(nested)));

    let mut root = Record::default();
    root.insert("id".to_string(), Value::String("snapshot-mixed".into()));
    root.insert("normal".to_string(), Value::Record(Arc::new(normal)));
    root.insert("projected".to_string(), Value::Record(Arc::new(projected)));
    root.insert("mixed".to_string(), Value::Record(Arc::new(mixed)));
    Value::Record(Arc::new(root))
}

fn projected_operations_record() -> Value {
    let mut record = Record::default();
    record.insert(
        "items".to_string(),
        Value::Projected(ProjectedValue::custom(
            "proj.items",
            Arc::new(ProjectedList {
                name: "items",
                values: vec![
                    Value::String("alpha".into()),
                    Value::String("beta".into()),
                    Value::String("gamma".into()),
                    Value::String("delta".into()),
                ]
                .into(),
            }),
        )),
    );
    record.insert(
        "text".to_string(),
        Value::Projected(ProjectedValue::custom(
            "proj.text",
            Arc::new(ProjectedText::new("text", "alpha beta gamma delta")),
        )),
    );
    record.insert(
        "padded".to_string(),
        Value::Projected(ProjectedValue::custom(
            "proj.padded",
            Arc::new(ProjectedText::new("padded", "  alpha  ")),
        )),
    );
    record.insert(
        "json".to_string(),
        Value::Projected(ProjectedValue::custom(
            "proj.json",
            Arc::new(ProjectedText::new("json", "{\"ok\":true,\"count\":4}")),
        )),
    );
    record.insert(
        "number".to_string(),
        Value::Projected(ProjectedValue::scalar("proj.number", Value::Number(4.0))),
    );
    record.insert(
        "record".to_string(),
        Value::Projected(ProjectedValue::scalar("proj.record", {
            let mut inner = Record::default();
            inner.insert("topic".to_string(), Value::String("bench".into()));
            inner.insert("count".to_string(), Value::Number(4.0));
            Value::Record(Arc::new(inner))
        })),
    );
    Value::Record(Arc::new(record))
}

struct ProjectedList {
    name: &'static str,
    values: ListValue,
}

impl ProjectedList {
    fn history() -> Self {
        Self {
            name: "history",
            values: vec![
                history_item("user", "alpha request"),
                history_item("assistant", "beta response"),
                history_item("tool", "gamma observation"),
            ]
            .into(),
        }
    }
}

fn string_value(value: &Value) -> Result<compact_str::CompactString, ()> {
    match value {
        Value::String(value) => Ok(value.clone().into()),
        Value::Number(value) => Ok(value.to_string().into()),
        Value::Bool(value) => Ok(if *value { "true" } else { "false" }.into()),
        Value::Null => Ok("null".into()),
        _ => Err(()),
    }
}

fn clamp_slice_bounds(
    start: Option<isize>,
    end: Option<isize>,
    len: usize,
) -> Option<(usize, usize)> {
    let len = len as isize;
    let normalize = |bound: Option<isize>, default| {
        let bound = bound.unwrap_or(default);
        if bound < 0 { len + bound } else { bound }.clamp(0, len)
    };
    let start = normalize(start, 0);
    let end = normalize(end, len);
    (start < end).then_some((start as usize, end as usize))
}

fn slice_string(text: &str, start: Option<isize>, end: Option<isize>) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    let Some((start, end)) = clamp_slice_bounds(start, end, chars.len()) else {
        return String::new();
    };
    chars[start..end].iter().collect()
}

impl ProjectedHostDescriptor for ProjectedList {
    fn type_name(&self) -> &str {
        "list"
    }

    fn read_one(
        &self,
        request: ProjectedReadRequest,
    ) -> ProjectedFuture<'_, Option<ProjectedReadResponse>> {
        Box::pin(async move {
            let ProjectedReadRequest::Index(index) = request else {
                return match request {
                    ProjectedReadRequest::Len => {
                        Some(ProjectedReadResponse::Len(self.values.len()))
                    }
                    ProjectedReadRequest::Empty => {
                        Some(ProjectedReadResponse::Bool(self.values.is_empty()))
                    }
                    ProjectedReadRequest::Truthy => Some(ProjectedReadResponse::Bool(true)),
                    ProjectedReadRequest::Contains(needle) => Some(ProjectedReadResponse::Bool(
                        self.values.iter().any(|value| value == &needle),
                    )),
                    ProjectedReadRequest::Join(sep) => {
                        let Ok(sep) = string_value(&sep) else {
                            return None;
                        };
                        let mut joined = String::new();
                        for (index, value) in self.values.iter().enumerate() {
                            let Ok(value) = string_value(value) else {
                                return None;
                            };
                            if index > 0 {
                                joined.push_str(sep.as_ref());
                            }
                            joined.push_str(value.as_ref());
                        }
                        Some(ProjectedReadResponse::Value(Value::String(joined.into())))
                    }
                    ProjectedReadRequest::Slice { start, end } => {
                        let Some((start, end)) = clamp_slice_bounds(start, end, self.values.len())
                        else {
                            return Some(ProjectedReadResponse::Value(Value::List(
                                Vec::new().into(),
                            )));
                        };
                        Some(ProjectedReadResponse::Value(Value::List(
                            self.values[start..end].to_vec().into(),
                        )))
                    }
                    ProjectedReadRequest::Push(item) => {
                        let mut values = self.values.to_vec();
                        values.push(item);
                        Some(ProjectedReadResponse::Value(Value::List(values.into())))
                    }
                    ProjectedReadRequest::Render => Some(ProjectedReadResponse::Text(format!(
                        "<{}:{}>",
                        self.name,
                        self.values.len()
                    ))),
                    ProjectedReadRequest::Materialize => Some(ProjectedReadResponse::Value(
                        Value::List(self.values.clone()),
                    )),
                    _ => None,
                };
            };
            let index = resolve_index(&index, self.values.len())?;
            self.values
                .get(index)
                .cloned()
                .map(ProjectedReadResponse::Value)
        })
    }
}

fn history_item(role: &str, content: &str) -> Value {
    let mut record = Record::default();
    record.insert("role".to_string(), Value::String(role.to_string().into()));
    record.insert(
        "content".to_string(),
        Value::Projected(ProjectedValue::custom(
            format!("history.{role}.content"),
            Arc::new(ProjectedText::new("content", content)),
        )),
    );
    Value::Record(Arc::new(record))
}

struct ProjectedText {
    name: &'static str,
    text: Arc<str>,
}

impl ProjectedText {
    fn new(name: &'static str, text: impl Into<Arc<str>>) -> Self {
        Self {
            name,
            text: text.into(),
        }
    }
}

impl ProjectedHostDescriptor for ProjectedText {
    fn type_name(&self) -> &str {
        "string"
    }

    fn read_one(
        &self,
        request: ProjectedReadRequest,
    ) -> ProjectedFuture<'_, Option<ProjectedReadResponse>> {
        Box::pin(async move {
            match request {
                ProjectedReadRequest::Len => {
                    Some(ProjectedReadResponse::Len(self.text.chars().count()))
                }
                ProjectedReadRequest::Empty => {
                    Some(ProjectedReadResponse::Bool(self.text.is_empty()))
                }
                ProjectedReadRequest::Truthy => {
                    Some(ProjectedReadResponse::Bool(!self.text.is_empty()))
                }
                ProjectedReadRequest::Index(index) => {
                    let index = resolve_index(&index, self.text.chars().count())?;
                    self.text.chars().nth(index).map(|ch| {
                        ProjectedReadResponse::Value(Value::String(ch.to_compact_string().into()))
                    })
                }
                ProjectedReadRequest::Find { needle, start } => {
                    let Value::String(needle) = needle else {
                        return None;
                    };
                    Some(ProjectedReadResponse::Value(
                        match find_text(&self.text, &needle, start) {
                            Some(index) => Value::Number(index as f64),
                            None => Value::Null,
                        },
                    ))
                }
                ProjectedReadRequest::GrepText(needle) => {
                    let Value::String(needle) = needle else {
                        return None;
                    };
                    grep_text_records(&self.text, &needle).map(ProjectedReadResponse::Value)
                }
                ProjectedReadRequest::StartsWith(prefix) => {
                    let Value::String(prefix) = prefix else {
                        return None;
                    };
                    Some(ProjectedReadResponse::Bool(self.text.starts_with(&*prefix)))
                }
                ProjectedReadRequest::EndsWith(suffix) => {
                    let Value::String(suffix) = suffix else {
                        return None;
                    };
                    Some(ProjectedReadResponse::Bool(self.text.ends_with(&*suffix)))
                }
                ProjectedReadRequest::Split(needle) => {
                    let Value::String(needle) = needle else {
                        return None;
                    };
                    Some(ProjectedReadResponse::Value(Value::List(
                        self.text
                            .split(&*needle)
                            .map(|part| Value::String(part.into()))
                            .collect::<Vec<_>>()
                            .into(),
                    )))
                }
                ProjectedReadRequest::Trim => Some(ProjectedReadResponse::Value(Value::String(
                    self.text.trim().into(),
                ))),
                ProjectedReadRequest::Slice { start, end } => Some(ProjectedReadResponse::Value(
                    Value::String(slice_string(&self.text, start, end).into()),
                )),
                ProjectedReadRequest::ToNumber => self
                    .text
                    .parse::<f64>()
                    .ok()
                    .map(Value::Number)
                    .map(ProjectedReadResponse::Value),
                ProjectedReadRequest::JsonParse => {
                    serde_json::from_str::<serde_json::Value>(&self.text)
                        .ok()
                        .map(from_json)
                        .map(ProjectedReadResponse::Value)
                }
                ProjectedReadRequest::Render => Some(ProjectedReadResponse::Text(format!(
                    "<{}:{} chars>",
                    self.name,
                    self.text.chars().count()
                ))),
                ProjectedReadRequest::Materialize => Some(ProjectedReadResponse::Value(
                    Value::String(self.text.as_ref().into()),
                )),
                _ => None,
            }
        })
    }
}

fn grep_text_records(text: &str, needle: &str) -> Option<Value> {
    if needle.is_empty() {
        return None;
    }

    let needle_len = needle.chars().count();
    let needle_value = Value::String(needle.into());
    let mut matches = Vec::new();
    for (line_index, line) in text.lines().enumerate() {
        let Some(start) = find_text(line, needle, 0) else {
            continue;
        };
        let mut record = Record::with_capacity(5);
        record.insert_str("line", Value::Number((line_index + 1) as f64));
        record.insert_str("text", Value::String(line.into()));
        record.insert_str("match", needle_value.clone());
        record.insert_str("start", Value::Number(start as f64));
        record.insert_str("end", Value::Number((start + needle_len) as f64));
        matches.push(Value::Record(Arc::new(record)));
    }
    Some(Value::List(matches.into()))
}

fn find_text(text: &str, needle: &str, start: usize) -> Option<usize> {
    let start_byte = if start == 0 {
        0
    } else {
        byte_index_for_char(text, start)?
    };
    if needle.is_empty() {
        return Some(start);
    }
    let tail = &text[start_byte..];
    let match_byte = tail.find(needle)?;
    Some(start + tail[..match_byte].chars().count())
}

fn byte_index_for_char(text: &str, target: usize) -> Option<usize> {
    let mut char_count = 0;
    for (byte_index, _) in text.char_indices() {
        if char_count == target {
            return Some(byte_index);
        }
        char_count += 1;
    }
    (char_count == target).then_some(text.len())
}

fn resolve_index(index: &Value, len: usize) -> Option<usize> {
    let Value::Number(index) = index else {
        return None;
    };
    if !index.is_finite() || index.fract() != 0.0 {
        return None;
    }
    let len = len as isize;
    let index = *index as isize;
    let normalized = if index < 0 { len + index } else { index };
    (0..len)
        .contains(&normalized)
        .then_some(normalized as usize)
}

pub struct BenchHost;
