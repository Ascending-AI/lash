use super::{
    ImageValue, RuntimeError, Value,
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
    Primitive(PrimitiveMask),
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

#[derive(Clone, Copy, PartialEq, Eq)]
struct PrimitiveMask(u16);

impl PrimitiveMask {
    const STRING: Self = Self(1 << 0);
    const NUMBER: Self = Self(1 << 1);
    const INTEGER: Self = Self(1 << 2);
    const BOOLEAN: Self = Self(1 << 3);
    const ARRAY: Self = Self(1 << 4);
    const OBJECT: Self = Self(1 << 5);
    const NULL: Self = Self(1 << 6);

    fn from_schema_name(name: &str) -> Option<Self> {
        Some(match name {
            "string" => Self::STRING,
            "number" => Self::NUMBER,
            "integer" => Self::INTEGER,
            "boolean" => Self::BOOLEAN,
            "array" => Self::ARRAY,
            "object" => Self::OBJECT,
            "null" => Self::NULL,
            _ => return None,
        })
    }

    fn schema_name(self) -> &'static str {
        match self {
            Self::STRING => "string",
            Self::NUMBER => "number",
            Self::INTEGER => "integer",
            Self::BOOLEAN => "boolean",
            Self::ARRAY => "array",
            Self::OBJECT => "object",
            Self::NULL => "null",
            _ => "value",
        }
    }

    fn matches(self, value: &Value) -> bool {
        let value_mask = match value {
            Value::String(_) => Self::STRING.0,
            Value::Number(number) if number.is_finite() && number.fract() == 0.0 => {
                Self::NUMBER.0 | Self::INTEGER.0
            }
            Value::Number(number) if number.is_finite() => Self::NUMBER.0,
            Value::Bool(_) => Self::BOOLEAN.0,
            Value::Tuple(_) | Value::List(_) => Self::ARRAY.0,
            Value::Record(_) | Value::Image(_) | Value::Resource(_) => Self::OBJECT.0,
            Value::Null => Self::NULL.0,
            Value::Projected(value) => match value.value_type_name() {
                "tuple" | "list" => Self::ARRAY.0,
                _ => Self::OBJECT.0,
            },
            Value::Number(_) => 0,
        };
        self.0 & value_mask != 0
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
        Some(Value::String(expected)) => PrimitiveMask::from_schema_name(expected.as_str()),
        _ => None,
    };

    match schema_type {
        Some(PrimitiveMask::ARRAY) => {
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
        Some(PrimitiveMask::OBJECT) => ValidationPlan {
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
                if !PrimitiveMask::STRING.matches(value) {
                    return describe_primitive_failure(value, PrimitiveMask::STRING, path);
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
                if !PrimitiveMask::ARRAY.matches(value) {
                    return describe_primitive_failure(value, PrimitiveMask::ARRAY, path);
                }
                let items = match value {
                    Value::Tuple(items) | Value::List(items) => items,
                    _ => return describe_primitive_failure(value, PrimitiveMask::ARRAY, path),
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
                describe_primitive_failure(value, PrimitiveMask::ARRAY, path)
            }
            ValidationPlanKind::Object(fields) => {
                if !PrimitiveMask::OBJECT.matches(value) {
                    return describe_primitive_failure(value, PrimitiveMask::OBJECT, path);
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
                describe_primitive_failure(value, PrimitiveMask::OBJECT, path)
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
            ValidationPlanKind::Primitive(kind) => kind.schema_name().to_string(),
            ValidationPlanKind::Enum(values) => format!(
                "enum[{}]",
                values
                    .iter()
                    .map(AsRef::as_ref)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ValidationPlanKind::List(_) => "array".to_string(),
            ValidationPlanKind::Object(_) => "object".to_string(),
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
    expected: PrimitiveMask,
    path: &[PathSegment<'_>],
) -> String {
    format!(
        "{}: expected {}, got {}",
        format_schema_path(path),
        expected.schema_name(),
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
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Image(_) => "object",
        Value::Resource(_) => "object",
        Value::Tuple(_) | Value::List(_) => "array",
        Value::Record(_) => "object",
        Value::Projected(value) => match value.value_type_name() {
            "tuple" | "list" => "array",
            _ => "object",
        },
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
