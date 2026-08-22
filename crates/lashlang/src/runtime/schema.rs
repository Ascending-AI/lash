use super::{
    ImageValue, RuntimeError, Value, debug_assert_exported_value,
    record::{Symbol, intern_symbol},
    unwrap_type_value,
};
use smallvec::SmallVec;
use std::fmt::Write as _;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct ValidationPlan {
    kind: ValidationPlanKind,
}

#[derive(Clone)]
enum ValidationPlanKind {
    Any,
    Primitive(SchemaScalarKind),
    Enum(Box<[Arc<str>]>),
    List(Box<ValidationPlan>),
    Object(Box<[ValidationFieldPlan]>),
    Union(Box<[ValidationPlan]>),
}

#[derive(Clone)]
struct ValidationFieldPlan {
    symbol: Symbol,
    name: Arc<str>,
    required: bool,
    plan: ValidationPlan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
/// The single home of Lashlang's JSON-Schema scalar vocabulary.
pub(crate) enum SchemaScalarKind {
    String,
    Number,
    Integer,
    Boolean,
    Array,
    Object,
    Null,
}

impl SchemaScalarKind {
    pub(crate) fn from_schema_name(name: &str) -> Option<Self> {
        Some(match name {
            "string" => Self::String,
            "number" => Self::Number,
            "integer" => Self::Integer,
            "boolean" => Self::Boolean,
            "array" => Self::Array,
            "object" => Self::Object,
            "null" => Self::Null,
            _ => return None,
        })
    }

    pub(crate) const fn as_schema_name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
            Self::Array => "array",
            Self::Object => "object",
            Self::Null => "null",
        }
    }

    fn matches(self, value: &Value) -> bool {
        if matches!(value, Value::Ref(_)) {
            debug_assert_exported_value("schema validation");
            return false;
        }

        match self {
            Self::String => matches!(value, Value::String(_)),
            Self::Number => matches!(value, Value::Number(number) if number.is_finite()),
            Self::Integer => {
                matches!(value, Value::Number(number) if number.is_finite() && number.fract() == 0.0)
            }
            Self::Boolean => matches!(value, Value::Bool(_)),
            Self::Array => match value {
                Value::Tuple(_) | Value::List(_) => true,
                Value::Projected(value) => matches!(value.value_type_name(), "tuple" | "list"),
                _ => false,
            },
            Self::Object => match value {
                Value::Record(_) | Value::Image(_) | Value::Resource(_) => true,
                Value::Projected(value) => !matches!(value.value_type_name(), "tuple" | "list"),
                _ => false,
            },
            Self::Null => matches!(value, Value::Null),
        }
    }
}

pub(crate) fn execute_validate_builtin(
    value: Value,
    schema: &Value,
) -> Result<Value, RuntimeError> {
    let schema = unwrap_type_value(schema).ok_or(RuntimeError::ValidateTypeLiteralRequired)?;
    let plan = compile_schema_value(schema);
    execute_validation_plan(value, &plan)
}

pub(crate) fn execute_validation_plan(
    value: Value,
    plan: &ValidationPlan,
) -> Result<Value, RuntimeError> {
    if plan.accepts(&value) {
        return Ok(value);
    }

    let mut path = SmallVec::<[PathSegment<'_>; 8]>::new();
    let message = plan.describe_failure(&value, &mut path);
    Err(RuntimeError::ValidationFailed { reason: message })
}

pub(crate) fn compile_schema_value(schema: &Value) -> ValidationPlan {
    let Some(schema_obj) = schema.as_record() else {
        return ValidationPlan {
            kind: ValidationPlanKind::Any,
        };
    };

    if let Some(Value::List(variants)) = schema_obj.get("anyOf") {
        return ValidationPlan {
            kind: ValidationPlanKind::Union(
                variants
                    .iter()
                    .map(compile_schema_value)
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            ),
        };
    }

    if let Some(Value::List(allowed)) = schema_obj.get("enum") {
        return ValidationPlan {
            kind: ValidationPlanKind::Enum(
                allowed
                    .iter()
                    .filter_map(|value| match value {
                        Value::String(value) => Some(Arc::<str>::from(value.as_str())),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            ),
        };
    }

    let schema_type = match schema_obj.get("type") {
        Some(Value::String(expected)) => SchemaScalarKind::from_schema_name(expected.as_str()),
        _ => None,
    };

    match schema_type {
        Some(SchemaScalarKind::Array) => {
            let item_plan =
                schema_obj
                    .get("items")
                    .map(compile_schema_value)
                    .unwrap_or(ValidationPlan {
                        kind: ValidationPlanKind::Any,
                    });
            ValidationPlan {
                kind: ValidationPlanKind::List(Box::new(item_plan)),
            }
        }
        Some(SchemaScalarKind::Object) => ValidationPlan {
            kind: ValidationPlanKind::Object(compile_object_fields(schema_obj)),
        },
        Some(kind) => ValidationPlan {
            kind: ValidationPlanKind::Primitive(kind),
        },
        None => ValidationPlan {
            kind: ValidationPlanKind::Any,
        },
    }
}

impl ValidationPlan {
    fn accepts(&self, value: &Value) -> bool {
        match &self.kind {
            ValidationPlanKind::Any => true,
            ValidationPlanKind::Primitive(expected) => expected.matches(value),
            ValidationPlanKind::Enum(allowed) => {
                let Value::String(value) = value else {
                    return false;
                };
                allowed.iter().any(|candidate| candidate.as_ref() == value)
            }
            ValidationPlanKind::List(item_plan) => {
                let items = match value {
                    Value::Tuple(items) | Value::List(items) => items,
                    _ => {
                        return false;
                    }
                };
                items.iter().all(|item| item_plan.accepts(item))
            }
            ValidationPlanKind::Object(fields) => match value {
                Value::Record(record) => fields.iter().all(|field| {
                    record
                        .get_symbol(field.symbol)
                        .map_or(!field.required, |field_value| {
                            field.plan.accepts(field_value)
                        })
                }),
                Value::Image(image) => fields.iter().all(|field| {
                    image_field_value(image, field.name.as_ref())
                        .map_or(!field.required, |field_value| {
                            field.plan.accepts(&field_value)
                        })
                }),
                _ => false,
            },
            ValidationPlanKind::Union(variants) => {
                variants.iter().any(|variant| variant.accepts(value))
            }
        }
    }

    fn describe_failure<'a>(
        &'a self,
        value: &Value,
        path: &mut SmallVec<[PathSegment<'a>; 8]>,
    ) -> String {
        match &self.kind {
            ValidationPlanKind::Any => format!(
                "{}: expected any, got {}",
                format_schema_path(path),
                schema_value_type_name(value)
            ),
            ValidationPlanKind::Primitive(expected) => {
                describe_primitive_failure(value, *expected, path)
            }
            ValidationPlanKind::Enum(allowed) => {
                if !SchemaScalarKind::String.matches(value) {
                    return describe_primitive_failure(value, SchemaScalarKind::String, path);
                }
                let allowed = allowed
                    .iter()
                    .map(AsRef::as_ref)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "{}: expected one of [{allowed}], got {value}",
                    format_schema_path(path)
                )
            }
            ValidationPlanKind::List(item_plan) => {
                if !SchemaScalarKind::Array.matches(value) {
                    return describe_primitive_failure(value, SchemaScalarKind::Array, path);
                }
                let items = match value {
                    Value::Tuple(items) | Value::List(items) => items,
                    _ => return describe_primitive_failure(value, SchemaScalarKind::Array, path),
                };
                for (index, item) in items.iter().enumerate() {
                    path.push(PathSegment::Index(index));
                    if !item_plan.accepts(item) {
                        let message = item_plan.describe_failure(item, path);
                        path.pop();
                        return message;
                    }
                    path.pop();
                }
                describe_primitive_failure(value, SchemaScalarKind::Array, path)
            }
            ValidationPlanKind::Object(fields) => {
                if !SchemaScalarKind::Object.matches(value) {
                    return describe_primitive_failure(value, SchemaScalarKind::Object, path);
                }
                for field in fields.iter() {
                    let field_value = plan_field_value(value, field);
                    let Some(field_value) = field_value else {
                        if field.required {
                            return format!(
                                "{}: missing required field `{}`",
                                format_schema_path(path),
                                field.name
                            );
                        }
                        continue;
                    };
                    if !field.plan.accepts(field_value.as_ref()) {
                        path.push(PathSegment::Field(field.name.as_ref()));
                        let message = field.plan.describe_failure(field_value.as_ref(), path);
                        path.pop();
                        return message;
                    }
                }
                describe_primitive_failure(value, SchemaScalarKind::Object, path)
            }
            ValidationPlanKind::Union(variants) => format!(
                "{}: expected one of [{}], got {}",
                format_schema_path(path),
                variants
                    .iter()
                    .map(ValidationPlan::describe)
                    .collect::<Vec<_>>()
                    .join(", "),
                schema_value_type_name(value)
            ),
        }
    }

    fn describe(&self) -> String {
        match &self.kind {
            ValidationPlanKind::Any => "any".to_string(),
            ValidationPlanKind::Primitive(kind) => kind.as_schema_name().to_string(),
            ValidationPlanKind::Enum(values) => format!(
                "enum[{}]",
                values
                    .iter()
                    .map(AsRef::as_ref)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ValidationPlanKind::List(_) => SchemaScalarKind::Array.as_schema_name().to_string(),
            ValidationPlanKind::Object(_) => SchemaScalarKind::Object.as_schema_name().to_string(),
            ValidationPlanKind::Union(variants) => variants
                .iter()
                .map(ValidationPlan::describe)
                .collect::<Vec<_>>()
                .join(" | "),
        }
    }
}

fn compile_object_fields(schema_obj: &super::Record) -> Box<[ValidationFieldPlan]> {
    let required_symbols = match schema_obj.get("required") {
        Some(Value::List(required)) => required
            .iter()
            .filter_map(|field| match field {
                Value::String(name) => Some((intern_symbol(name.as_str()), name.as_str())),
                _ => None,
            })
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };

    let mut fields = match schema_obj.get("properties") {
        Some(Value::Record(properties)) => properties
            .entries
            .iter()
            .map(|entry| ValidationFieldPlan {
                symbol: entry.symbol,
                name: entry.name.clone(),
                required: required_symbols
                    .iter()
                    .any(|(symbol, _)| *symbol == entry.symbol),
                plan: compile_schema_value(&entry.value),
            })
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };

    for (symbol, name) in required_symbols {
        if fields.iter().any(|field| field.symbol == symbol) {
            continue;
        }
        fields.push(ValidationFieldPlan {
            symbol,
            name: Arc::<str>::from(name),
            required: true,
            plan: ValidationPlan {
                kind: ValidationPlanKind::Any,
            },
        });
    }

    fields.into_boxed_slice()
}

enum FieldValue<'a> {
    Borrowed(&'a Value),
    Owned(Value),
}

impl AsRef<Value> for FieldValue<'_> {
    fn as_ref(&self) -> &Value {
        match self {
            Self::Borrowed(value) => value,
            Self::Owned(value) => value,
        }
    }
}

fn plan_field_value<'a>(value: &'a Value, field: &ValidationFieldPlan) -> Option<FieldValue<'a>> {
    match value {
        Value::Record(record) => record.get_symbol(field.symbol).map(FieldValue::Borrowed),
        Value::Image(image) => image_field_value(image, field.name.as_ref()).map(FieldValue::Owned),
        _ => None,
    }
}

fn describe_primitive_failure(
    value: &Value,
    expected: SchemaScalarKind,
    path: &[PathSegment<'_>],
) -> String {
    format!(
        "{}: expected {}, got {}",
        format_schema_path(path),
        expected.as_schema_name(),
        schema_value_type_name(value)
    )
}

#[derive(Clone, Copy)]
enum PathSegment<'a> {
    Field(&'a str),
    Index(usize),
}

fn format_schema_path(path: &[PathSegment<'_>]) -> String {
    let mut formatted = "$".to_string();
    for segment in path {
        match segment {
            PathSegment::Field(name) => {
                formatted.push('.');
                formatted.push_str(name);
            }
            PathSegment::Index(index) => {
                write!(formatted, "[{index}]").expect("string writes should not fail");
            }
        }
    }
    formatted
}

fn schema_value_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => SchemaScalarKind::Null.as_schema_name(),
        Value::Undefined => "undefined",
        Value::Bool(_) => SchemaScalarKind::Boolean.as_schema_name(),
        Value::Number(_) => SchemaScalarKind::Number.as_schema_name(),
        Value::String(_) => SchemaScalarKind::String.as_schema_name(),
        Value::Image(_) => SchemaScalarKind::Object.as_schema_name(),
        Value::Resource(_) => SchemaScalarKind::Object.as_schema_name(),
        Value::Tuple(_) | Value::List(_) => SchemaScalarKind::Array.as_schema_name(),
        Value::Record(_) => SchemaScalarKind::Object.as_schema_name(),
        Value::Projected(value) => match value.value_type_name() {
            "tuple" | "list" => SchemaScalarKind::Array.as_schema_name(),
            _ => SchemaScalarKind::Object.as_schema_name(),
        },
        Value::Ref(_) => "heap_ref",
    }
}

fn image_field_value(image: &ImageValue, field: &str) -> Option<Value> {
    match field {
        "type" => Some(Value::String("image".into())),
        "id" => Some(Value::String(image.id.clone().into())),
        "label" => Some(Value::String(image.label.clone().into())),
        "size" => Some(Value::Number(image.size as f64)),
        "width" => Some(
            image
                .width
                .map(|width| Value::Number(width as f64))
                .unwrap_or(Value::Null),
        ),
        "height" => Some(
            image
                .height
                .map(|height| Value::Number(height as f64))
                .unwrap_or(Value::Null),
        ),
        _ => None,
    }
}
