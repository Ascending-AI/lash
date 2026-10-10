//! The shape of a value too long for history (FIG-1439).
//!
//! A code mode value retained out of history leaves its program a
//! descriptor in its place, not a cut of its text: the value's type
//! skeleton, its sizes, and the keys its objects were observed to hold. A
//! program reads what the value is made of from it and asks for the value
//! itself only where it needs it.
//!
//! The descriptor is bounded whatever the value weighs. Elements of an array
//! are described together, as the merge of what each element is; nesting
//! below [`MAX_DEPTH`], keys past [`MAX_FIELDS`] per object and keys longer
//! than [`MAX_KEY_BYTES`] are counted, not described; and past [`MAX_NODES`]
//! descriptions, the deepest containers are counted the same way, so the
//! outermost keys are always the ones described.

use serde::{Deserialize, Serialize};

/// The deepest nesting a descriptor describes: a container at this depth
/// keeps its sizes and loses its contents.
pub const MAX_DEPTH: usize = 4;
/// The most keys one object's descriptor names.
pub const MAX_FIELDS: usize = 32;
/// The longest key a descriptor names, in bytes.
pub const MAX_KEY_BYTES: usize = 64;
/// The most shapes one descriptor holds, itself included.
pub const MAX_NODES: usize = 96;

/// What a JSON value is made of. A position that held several values — the
/// elements of an array — is their merge: its sizes are the largest
/// observed, an object key not every one of them held is `optional`, and a
/// position whose values differ in type is [`ValueShape::OneOf`] them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ValueShape {
    Null,
    Boolean,
    Number,
    String {
        /// The string's length in UTF-8 bytes.
        bytes: u64,
    },
    Array {
        length: u64,
        /// The shape of the elements; absent for an empty array and for one
        /// whose contents the bound counts.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        items: Option<Box<ValueShape>>,
    },
    Object {
        /// The described keys, in the order first observed.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        fields: Vec<FieldShape>,
        /// Keys the bound counted instead of describing.
        #[serde(default, skip_serializing_if = "is_zero")]
        omitted: u64,
    },
    /// Values of more than one type, one shape per type in the order first
    /// observed.
    OneOf {
        shapes: Vec<ValueShape>,
    },
}

/// One key of an object's shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FieldShape {
    pub key: String,
    pub shape: ValueShape,
    /// Some object merged at this position did not hold the key.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn count(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

impl ValueShape {
    /// The bounded descriptor of `value`.
    pub fn of(value: &serde_json::Value) -> Self {
        let mut shape = Self::describe(value, 0);
        shape.bound_nodes();
        shape
    }

    fn describe(value: &serde_json::Value, depth: usize) -> Self {
        match value {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(_) => Self::Boolean,
            serde_json::Value::Number(_) => Self::Number,
            serde_json::Value::String(text) => Self::String {
                bytes: count(text.len()),
            },
            serde_json::Value::Array(elements) => Self::Array {
                length: count(elements.len()),
                items: (depth < MAX_DEPTH)
                    .then(|| {
                        elements
                            .iter()
                            .map(|element| Self::describe(element, depth + 1))
                            .reduce(Self::merge)
                    })
                    .flatten()
                    .map(Box::new),
            },
            serde_json::Value::Object(entries) => {
                if depth >= MAX_DEPTH {
                    return Self::Object {
                        fields: Vec::new(),
                        omitted: count(entries.len()),
                    };
                }
                let mut fields = Vec::new();
                let mut omitted = 0;
                for (key, entry) in entries {
                    if fields.len() < MAX_FIELDS && key.len() <= MAX_KEY_BYTES {
                        fields.push(FieldShape {
                            key: key.clone(),
                            shape: Self::describe(entry, depth + 1),
                            optional: false,
                        });
                    } else {
                        omitted += 1;
                    }
                }
                Self::Object { fields, omitted }
            }
        }
    }

    /// Which of the six JSON types a shape is of; `None` for a merge of
    /// several.
    fn type_index(&self) -> Option<u8> {
        match self {
            Self::Null => Some(0),
            Self::Boolean => Some(1),
            Self::Number => Some(2),
            Self::String { .. } => Some(3),
            Self::Array { .. } => Some(4),
            Self::Object { .. } => Some(5),
            Self::OneOf { .. } => None,
        }
    }

    /// The shape of a position that held a value of each shape.
    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::OneOf { mut shapes }, other) => {
                let others = match other {
                    Self::OneOf { shapes } => shapes,
                    single => vec![single],
                };
                for other in others {
                    match shapes
                        .iter()
                        .position(|shape| shape.type_index() == other.type_index())
                    {
                        Some(at) => {
                            let merged = std::mem::replace(&mut shapes[at], Self::Null);
                            shapes[at] = merged.merge(other);
                        }
                        None => shapes.push(other),
                    }
                }
                Self::OneOf { shapes }
            }
            (single, other @ Self::OneOf { .. }) => Self::OneOf {
                shapes: vec![single],
            }
            .merge(other),
            (Self::String { bytes: left }, Self::String { bytes: right }) => Self::String {
                bytes: left.max(right),
            },
            (
                Self::Array {
                    length: left,
                    items: left_items,
                },
                Self::Array {
                    length: right,
                    items: right_items,
                },
            ) => Self::Array {
                length: left.max(right),
                items: match (left_items, right_items) {
                    (Some(left), Some(right)) => Some(Box::new(left.merge(*right))),
                    (left, right) => left.or(right),
                },
            },
            (
                Self::Object {
                    fields: mut merged,
                    omitted: left,
                },
                Self::Object {
                    fields: others,
                    omitted: right,
                },
            ) => {
                let mut extra = 0;
                let mut seen = vec![false; merged.len()];
                for other in others {
                    match merged.iter().position(|field| field.key == other.key) {
                        Some(at) => {
                            seen[at] = true;
                            let shape = std::mem::replace(&mut merged[at].shape, Self::Null);
                            merged[at].shape = shape.merge(other.shape);
                            merged[at].optional |= other.optional;
                        }
                        None if merged.len() < MAX_FIELDS => {
                            merged.push(FieldShape {
                                optional: true,
                                ..other
                            });
                            seen.push(true);
                        }
                        None => extra += 1,
                    }
                }
                for (field, seen) in merged.iter_mut().zip(seen) {
                    field.optional |= !seen;
                }
                Self::Object {
                    fields: merged,
                    omitted: left.max(right).saturating_add(extra),
                }
            }
            (left, right) if left.type_index() == right.type_index() => left,
            (left, right) => Self::OneOf {
                shapes: vec![left, right],
            },
        }
    }

    /// The containers inside this shape, children first-observed first.
    fn children_mut(&mut self) -> Vec<&mut Self> {
        match self {
            Self::Array {
                items: Some(items), ..
            } => vec![items.as_mut()],
            Self::Object { fields, .. } => {
                fields.iter_mut().map(|field| &mut field.shape).collect()
            }
            Self::OneOf { shapes } => shapes.iter_mut().collect(),
            _ => Vec::new(),
        }
    }

    /// Counts what lies past [`MAX_NODES`] shapes, outermost first: a
    /// container whose contents would pass the bound keeps its sizes and
    /// loses its contents.
    fn bound_nodes(&mut self) {
        let mut kept = 1;
        let mut level = vec![self];
        while !level.is_empty() {
            let mut next = Vec::new();
            for shape in level {
                let children = shape.children_mut().len();
                if kept + children > MAX_NODES {
                    shape.drop_contents();
                    continue;
                }
                kept += children;
                next.extend(shape.children_mut());
            }
            level = next;
        }
    }

    fn drop_contents(&mut self) {
        match self {
            Self::Array { items, .. } => *items = None,
            Self::Object { fields, omitted } => {
                *omitted = omitted.saturating_add(count(fields.len()));
                fields.clear();
            }
            // A merge keeps its types, each one's contents counted.
            Self::OneOf { shapes } => shapes.iter_mut().for_each(Self::drop_contents),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The descriptor of rows a cell built: the type skeleton, the sizes and
    /// the keys observed, with a key only some rows hold optional.
    #[test]
    fn rows_are_described_by_their_skeleton_sizes_and_observed_keys() {
        let rows: Vec<_> = (0..3000)
            .map(|index| {
                if index == 7 {
                    json!({"index": index, "text": "seven", "flag": true})
                } else {
                    json!({"index": index, "text": "a row"})
                }
            })
            .collect();
        let shape = ValueShape::of(&json!({"rows": rows, "total": 3000}));
        assert_eq!(
            serde_json::to_value(&shape).expect("encodes"),
            json!({
                "type": "object",
                "fields": [
                    {"key": "rows", "shape": {
                        "type": "array",
                        "length": 3000,
                        "items": {"type": "object", "fields": [
                            {"key": "index", "shape": {"type": "number"}},
                            {"key": "text", "shape": {"type": "string", "bytes": 5}},
                            {"key": "flag", "shape": {"type": "boolean"}, "optional": true},
                        ]},
                    }},
                    {"key": "total", "shape": {"type": "number"}},
                ],
            })
        );
        let decoded: ValueShape =
            serde_json::from_value(serde_json::to_value(&shape).expect("encodes"))
                .expect("decodes");
        assert_eq!(decoded, shape);
    }

    /// Elements of different types are each described, in the order first
    /// observed.
    #[test]
    fn mixed_elements_merge_into_one_shape_per_type() {
        let shape = ValueShape::of(&json!([1, "ab", null, "abcd", [true], {"k": 1}, 2]));
        assert_eq!(
            serde_json::to_value(&shape).expect("encodes"),
            json!({"type": "array", "length": 7, "items": {"type": "one_of", "shapes": [
                {"type": "number"},
                {"type": "string", "bytes": 4},
                {"type": "null"},
                {"type": "array", "length": 1, "items": {"type": "boolean"}},
                {"type": "object", "fields": [{"key": "k", "shape": {"type": "number"}}]},
            ]}})
        );
    }

    fn node_count(shape: &ValueShape) -> usize {
        1 + match shape {
            ValueShape::Array {
                items: Some(items), ..
            } => node_count(items),
            ValueShape::Object { fields, .. } => {
                fields.iter().map(|field| node_count(&field.shape)).sum()
            }
            ValueShape::OneOf { shapes } => shapes.iter().map(node_count).sum(),
            _ => 0,
        }
    }

    /// However wide or deep the value, the descriptor stays within its
    /// bounds and the outermost keys are the described ones.
    #[test]
    fn the_descriptor_is_bounded_and_keeps_the_outermost_keys() {
        let wide: serde_json::Map<String, serde_json::Value> = (0..200)
            .map(|key| {
                let inner: serde_json::Map<_, _> = (0..20)
                    .map(|inner| (format!("inner{inner:02}"), json!({"leaf": [inner]})))
                    .collect();
                (format!("key{key:03}"), serde_json::Value::Object(inner))
            })
            .chain([("k".repeat(MAX_KEY_BYTES + 1), json!(1))])
            .collect();
        let shape = ValueShape::of(&serde_json::Value::Object(wide));
        assert!(node_count(&shape) <= MAX_NODES, "{}", node_count(&shape));
        let ValueShape::Object { fields, omitted } = &shape else {
            panic!("an object: {shape:?}");
        };
        assert_eq!(fields.len(), MAX_FIELDS);
        assert_eq!(fields[0].key, "key000");
        assert_eq!(*omitted, 201 - MAX_FIELDS as u64);
        let ValueShape::Object { fields, omitted } = &fields[MAX_FIELDS - 1].shape else {
            panic!("an object");
        };
        assert!(
            fields.is_empty(),
            "contents past the node bound are counted"
        );
        assert_eq!(*omitted, 20);

        let mut deep = json!("leaf");
        for _ in 0..10 {
            deep = json!({"next": deep});
        }
        let mut shape = &ValueShape::of(&deep);
        for _ in 0..MAX_DEPTH {
            let ValueShape::Object { fields, .. } = shape else {
                panic!("an object: {shape:?}");
            };
            shape = &fields[0].shape;
        }
        assert_eq!(
            shape,
            &ValueShape::Object {
                fields: Vec::new(),
                omitted: 1
            }
        );
    }
}
