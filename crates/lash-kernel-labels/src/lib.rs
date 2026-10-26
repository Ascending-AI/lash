//! Package-owned display facts, without execution semantics.
//!
//! The package crate owns each display entry and its key. This crate supplies
//! the plain data and the JSON label reader; it owns no library inventory.

use std::collections::BTreeMap;

/// Display facts for one package member or one private library definition.
///
/// Each comparison holds for exactly the arguments the function answers
/// `true` for, under its own equality, order or truth, in parameter order.
/// A reply receiver takes one effect reply and returns that reply in its
/// dialect's representation; it neither selects a part nor converts its kind.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MemberDisplay {
    pub title: String,
    pub comparison: Option<Comparison>,
    pub reply: bool,
}

impl MemberDisplay {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            comparison: None,
            reply: false,
        }
    }
}

/// What a comparison function decides. A binary comparison takes its left
/// operand first: `Contains` holds when the first argument holds the
/// second, `StartsWith` when the first begins with the second.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Comparison {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// The negation of its one operand's truth.
    Not,
    Contains,
    StartsWith,
    EndsWith,
}

impl Comparison {
    /// How many arguments the comparison reads.
    pub fn arity(self) -> usize {
        match self {
            Self::Not => 1,
            Self::Eq
            | Self::Ne
            | Self::Lt
            | Self::Le
            | Self::Gt
            | Self::Ge
            | Self::Contains
            | Self::StartsWith
            | Self::EndsWith => 2,
        }
    }
}

/// A refused labels document, located by JSON pointer rather than source line.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LabelError {
    pub pointer: String,
    pub key: String,
    pub message: String,
}

impl LabelError {
    pub fn at(parent: &str, key: &str, message: impl Into<String>) -> Self {
        Self {
            pointer: format!("{parent}/{}", key.replace('~', "~0").replace('/', "~1")),
            key: key.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for LabelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "labels at `{}` (key `{}`): {}",
            self.pointer, self.key, self.message
        )
    }
}
impl std::error::Error for LabelError {}

/// Parsed display facts; the package owner validates member and definition keys.
#[derive(Debug, Default)]
pub struct Labels {
    pub members: BTreeMap<String, MemberDisplay>,
    pub definitions: BTreeMap<String, MemberDisplay>,
}

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Document {
    #[serde(default)]
    members: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    definitions: BTreeMap<String, serde_json::Value>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Details {
    title: String,
    #[serde(default)]
    comparison: Option<Comparison>,
    #[serde(default)]
    reply: bool,
}

/// Read one JSON labels document. An absent source has no display facts.
/// Duplicate keys are rejected before any JSON object can discard an entry.
pub fn read_json(data: &str) -> Result<Labels, LabelError> {
    if data.is_empty() {
        return Ok(Labels::default());
    }
    let mut failure = None;
    let mut deserializer = serde_json::Deserializer::from_str(data);
    use serde::de::DeserializeSeed;
    let value = UniqueValue {
        pointer: String::new(),
        failure: &mut failure,
    }
    .deserialize(&mut deserializer)
    .and_then(|value| {
        deserializer.end()?;
        Ok(value)
    })
    .map_err(|_| {
        failure.unwrap_or_else(|| LabelError {
            pointer: String::new(),
            key: String::new(),
            message: "invalid JSON labels document".into(),
        })
    })?;
    let document: Document = decode(value, "")?;
    let entries = |section: &str, values: BTreeMap<String, serde_json::Value>| {
        values
            .into_iter()
            .map(|(key, value)| {
                let at = LabelError::at(section, &key, "");
                let shorthand = value.is_string();
                let details = match value {
                    serde_json::Value::String(title) => Details {
                        title,
                        comparison: None,
                        reply: false,
                    },
                    value => decode::<Details>(value, &at.pointer)?,
                };
                if details.title.trim().is_empty() {
                    return Err(if shorthand {
                        LabelError {
                            message: "expected a non-empty title".into(),
                            ..at
                        }
                    } else {
                        LabelError::at(&at.pointer, "title", "expected a non-empty title")
                    });
                }
                Ok((
                    key,
                    MemberDisplay {
                        title: details.title,
                        comparison: details.comparison,
                        reply: details.reply,
                    },
                ))
            })
            .collect::<Result<BTreeMap<_, _>, LabelError>>()
    };
    Ok(Labels {
        members: entries("/members", document.members)?,
        definitions: entries("/definitions", document.definitions)?,
    })
}

fn decode<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
    parent: &str,
) -> Result<T, LabelError> {
    if !value.is_object() {
        return Err(LabelError {
            pointer: parent.into(),
            key: parent
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .replace("~1", "/")
                .replace("~0", "~"),
            message: "expected a JSON object".into(),
        });
    }
    serde_path_to_error::deserialize(value).map_err(|error| {
        let mut pointer = parent.to_owned();
        let mut key = parent
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .replace("~1", "/")
            .replace("~0", "~");
        for segment in error.path() {
            key = match segment {
                serde_path_to_error::Segment::Map { key } => key.clone(),
                serde_path_to_error::Segment::Seq { index } => index.to_string(),
                serde_path_to_error::Segment::Enum { variant } => variant.clone(),
                serde_path_to_error::Segment::Unknown => continue,
            };
            pointer = LabelError::at(&pointer, &key, "").pointer;
        }
        LabelError {
            pointer,
            key,
            message: error.inner().to_string(),
        }
    })
}

/// A JSON value reader that owns each map insertion, retaining the path of
/// a duplicate before serde_json::Value could replace its first occurrence.
struct UniqueValue<'a> {
    pointer: String,
    failure: &'a mut Option<LabelError>,
}

impl<'de> serde::de::DeserializeSeed<'de> for UniqueValue<'_> {
    type Value = serde_json::Value;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> serde::de::Visitor<'de> for UniqueValue<'_> {
    type Value = serde_json::Value;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON labels value")
    }
    fn visit_map<M: serde::de::MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let at = LabelError::at(&self.pointer, &key, "duplicate key");
            if values.contains_key(&key) {
                *self.failure = Some(at);
                return Err(serde::de::Error::custom("duplicate key"));
            }
            let value = map.next_value_seed(UniqueValue {
                pointer: at.pointer,
                failure: self.failure,
            })?;
            values.insert(key, value);
        }
        Ok(serde_json::Value::Object(values))
    }
    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(serde_json::Value::String(value.into()))
    }
    fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
        Ok(serde_json::Value::String(value))
    }
    fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Bool(value))
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }
    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
        Ok(value.into())
    }
    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
        Ok(value.into())
    }
    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
        Ok(serde_json::Value::from(value))
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(UniqueValue {
            pointer: format!("{}/{}", self.pointer, values.len()),
            failure: self.failure,
        })? {
            values.push(value);
        }
        Ok(serde_json::Value::Array(values))
    }
}
