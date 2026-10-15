//! The one bounded writer every host or plugin contribution to a span goes
//! through. It charges counts and bytes before it buffers, against the one
//! budget a span has after the registry's reserve for lash's own
//! attributes, and reports what it refused as a typed [`TraceOmission`].

use std::borrow::Cow;

use crate::TraceSpanKind;
use crate::telemetry::metrics::registry::{AttributeKey, AttributeMutability, RefinableKey};

use super::{TraceLimits, TraceOmission, TraceOmissionCounts};

/// A contributed attribute key: non-empty, at most
/// `trace_limits.attribute_key_bytes`, ASCII graphic, never in the reserved
/// `lash.` namespace and never a key the registry declares. It is checked
/// when written, so a bad key is an omission, never a panic.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AttrKey(Cow<'static, str>);

impl AttrKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&'static str> for AttrKey {
    fn from(key: &'static str) -> Self {
        Self(Cow::Borrowed(key))
    }
}

impl From<String> for AttrKey {
    fn from(key: String) -> Self {
        Self(Cow::Owned(key))
    }
}

impl From<Cow<'static, str>> for AttrKey {
    fn from(key: Cow<'static, str>) -> Self {
        Self(key)
    }
}

/// A contributed value. `Json` is the declared JSON exception of the
/// tracing seam.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum TraceValue {
    Str(Cow<'static, str>),
    Int(i64),
    Float(f64),
    Bool(bool),
    StrArray(Vec<Cow<'static, str>>),
    Json(serde_json::Value),
}

impl TraceValue {
    /// The value retained on a record, before OTel's string encoding of JSON.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Str(v) => serde_json::json!(v),
            Self::Int(v) => serde_json::json!(v),
            Self::Float(v) => serde_json::json!(v),
            Self::Bool(v) => serde_json::json!(v),
            Self::StrArray(v) => serde_json::json!(v),
            Self::Json(v) => v.clone(),
        }
    }

    /// The bytes the value charges: UTF-8 length of a string, 8 for a
    /// number or a boolean, each element's length plus one for an array, and
    /// the serialized length of JSON.
    pub fn encoded_len(&self) -> u64 {
        let len = match self {
            Self::Str(value) => value.len(),
            Self::Int(_) | Self::Float(_) | Self::Bool(_) => 8,
            Self::StrArray(values) => values.iter().map(|value| value.len() + 1).sum(),
            Self::Json(value) => serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len()),
        };
        u64::try_from(len).unwrap_or(u64::MAX)
    }
}

impl From<&'static str> for TraceValue {
    fn from(value: &'static str) -> Self {
        Self::Str(Cow::Borrowed(value))
    }
}
impl From<String> for TraceValue {
    fn from(value: String) -> Self {
        Self::Str(Cow::Owned(value))
    }
}
impl From<Cow<'static, str>> for TraceValue {
    fn from(value: Cow<'static, str>) -> Self {
        Self::Str(value)
    }
}
impl From<i64> for TraceValue {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}
impl From<f64> for TraceValue {
    fn from(value: f64) -> Self {
        Self::Float(value)
    }
}
impl From<bool> for TraceValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}
impl From<Vec<String>> for TraceValue {
    fn from(values: Vec<String>) -> Self {
        Self::StrArray(values.into_iter().map(Cow::Owned).collect())
    }
}
impl From<serde_json::Value> for TraceValue {
    fn from(value: serde_json::Value) -> Self {
        Self::Json(value)
    }
}

/// What is left of a span's contribution budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriterRemaining {
    pub attributes: u32,
    pub bytes: u64,
}

/// One span's contributions as the writer admitted them, in admission
/// order: the attributes it added, the semantic defaults it replaced, and
/// its rename.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Contributions {
    pub added: Vec<(AttrKey, TraceValue)>,
    pub replaced: Vec<(RefinableKey, TraceValue)>,
    pub name: Option<Cow<'static, str>>,
}

/// Charges before it buffers and never buffers part of a value.
///
/// One `set` charges one attribute and `key.len() + encoded_len(value)`
/// bytes against what is left of the span's budget, all or nothing.
#[derive(Clone, Debug)]
pub struct TraceAttributeWriter {
    limits: TraceLimits,
    remaining: WriterRemaining,
    contributions: Contributions,
    omissions: TraceOmissionCounts,
}

impl TraceAttributeWriter {
    /// A writer over the whole contribution budget of a `kind` span:
    /// `attributes_per_span` less the registry's reserve for the kind, and
    /// `span_contribution_bytes`.
    pub fn new(kind: TraceSpanKind, limits: &TraceLimits) -> Self {
        Self {
            limits: *limits,
            remaining: WriterRemaining {
                attributes: limits.contribution_attributes(kind),
                bytes: limits.span_contribution_bytes(),
            },
            contributions: Contributions::default(),
            omissions: TraceOmissionCounts::default(),
        }
    }

    /// Adds a contributed attribute.
    ///
    /// A key the registry declares is refused: a provenance key with
    /// `Immutable`, a refinable one with `UseReplace`. A string over
    /// `attribute_value_bytes` is kept cut at a UTF-8 boundary and reported
    /// `ValueTruncated`; a JSON value over it is refused whole.
    pub fn set(
        &mut self,
        key: impl Into<AttrKey>,
        value: impl Into<TraceValue>,
    ) -> Result<(), TraceOmission> {
        let key = key.into();
        let result = self.admit(key, value.into());
        self.count(result)
    }

    /// Replaces a semantic default lash or a body set. The key is already
    /// in the reserve, so a replacement charges bytes only.
    pub fn replace(
        &mut self,
        key: RefinableKey,
        value: impl Into<TraceValue>,
    ) -> Result<(), TraceOmission> {
        let result = self.admit_replacement(key, value.into());
        self.count(result)
    }

    /// Renames the span: at most `name_bytes`, charged to the span's bytes.
    pub fn rename(&mut self, name: impl Into<Cow<'static, str>>) -> Result<(), TraceOmission> {
        let name = name.into();
        let result = self.admit_name(name);
        self.count(result)
    }

    /// Attributes and bytes left in this span's contribution budget.
    pub fn remaining(&self) -> WriterRemaining {
        self.remaining
    }

    /// What the writer admitted so far.
    pub fn contributions(&self) -> &Contributions {
        &self.contributions
    }

    /// What the writer refused so far, by reason.
    pub fn omissions(&self) -> TraceOmissionCounts {
        self.omissions
    }

    /// Counts `omission` against the span without a write: a contribution
    /// lash refused before it reached the writer, such as a panicking
    /// enricher's.
    pub fn omit(&mut self, omission: TraceOmission) {
        self.omissions.add(omission);
    }

    /// The admitted contributions and the omissions, for the span.
    pub fn finish(self) -> (Contributions, TraceOmissionCounts) {
        (self.contributions, self.omissions)
    }

    fn count(&mut self, result: Result<(), TraceOmission>) -> Result<(), TraceOmission> {
        if let Err(omission) = result {
            self.omissions.add(omission);
        }
        result
    }

    fn admit(&mut self, key: AttrKey, value: TraceValue) -> Result<(), TraceOmission> {
        let name = key.as_str();
        if name.is_empty()
            || u64::try_from(name.len()).unwrap_or(u64::MAX) > self.limits.attribute_key_bytes()
            || !name.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(TraceOmission::Key);
        }
        if name.starts_with("lash.") {
            return Err(TraceOmission::ReservedKey);
        }
        if let Some(attribute) = AttributeKey::of_key(name) {
            return Err(match attribute.definition().mutability {
                AttributeMutability::Provenance => TraceOmission::Immutable,
                AttributeMutability::Refinable => TraceOmission::UseReplace,
            });
        }
        let (value, truncated) = self.bounded(value)?;
        let earlier = self
            .contributions
            .added
            .iter()
            .position(|(kept, _)| kept == &key);
        let refund = earlier.map_or(0, |index| {
            let (kept, value) = &self.contributions.added[index];
            key_len(kept) + value.encoded_len()
        });
        let bytes = key_len(&key) + value.encoded_len();
        let slots = u32::from(earlier.is_none());
        if self.remaining.attributes < slots {
            return Err(TraceOmission::Attributes);
        }
        if self.remaining.bytes + refund < bytes {
            return Err(TraceOmission::SpanBytes);
        }
        self.remaining.attributes -= slots;
        self.remaining.bytes = self.remaining.bytes + refund - bytes;
        match earlier {
            Some(index) => self.contributions.added[index] = (key, value),
            None => self.contributions.added.push((key, value)),
        }
        truncated
    }

    fn admit_replacement(
        &mut self,
        key: RefinableKey,
        value: TraceValue,
    ) -> Result<(), TraceOmission> {
        let (value, truncated) = self.bounded(value)?;
        let earlier = self
            .contributions
            .replaced
            .iter()
            .position(|(kept, _)| *kept == key);
        let refund = earlier.map_or(0, |index| {
            self.contributions.replaced[index].1.encoded_len()
        });
        let bytes = value.encoded_len();
        if self.remaining.bytes + refund < bytes {
            return Err(TraceOmission::SpanBytes);
        }
        self.remaining.bytes = self.remaining.bytes + refund - bytes;
        match earlier {
            Some(index) => self.contributions.replaced[index] = (key, value),
            None => self.contributions.replaced.push((key, value)),
        }
        truncated
    }

    fn admit_name(&mut self, name: Cow<'static, str>) -> Result<(), TraceOmission> {
        let bytes = u64::try_from(name.len()).unwrap_or(u64::MAX);
        if name.is_empty() || bytes > self.limits.name_bytes() {
            return Err(TraceOmission::Name);
        }
        let refund = self
            .contributions
            .name
            .as_ref()
            .map_or(0, |name| u64::try_from(name.len()).unwrap_or(u64::MAX));
        if self.remaining.bytes + refund < bytes {
            return Err(TraceOmission::SpanBytes);
        }
        self.remaining.bytes = self.remaining.bytes + refund - bytes;
        self.contributions.name = Some(name);
        Ok(())
    }

    /// `value` within `attribute_value_bytes`: a string cut at a UTF-8
    /// boundary (reported `ValueTruncated` once kept), a non-finite float or
    /// an oversize JSON value refused.
    fn bounded(
        &self,
        value: TraceValue,
    ) -> Result<(TraceValue, Result<(), TraceOmission>), TraceOmission> {
        let limit = usize::try_from(self.limits.attribute_value_bytes()).unwrap_or(usize::MAX);
        let mut truncated = Ok(());
        let value = match value {
            TraceValue::Float(value) if !value.is_finite() => {
                return Err(TraceOmission::NonFinite);
            }
            TraceValue::Json(value) => {
                let json = TraceValue::Json(value);
                if json.encoded_len() > self.limits.attribute_value_bytes() {
                    return Err(TraceOmission::ValueTooLarge);
                }
                json
            }
            TraceValue::Str(value) => {
                let (value, cut) = cut(value, limit);
                if cut {
                    truncated = Err(TraceOmission::ValueTruncated);
                }
                TraceValue::Str(value)
            }
            TraceValue::StrArray(values) => TraceValue::StrArray(
                values
                    .into_iter()
                    .map(|value| {
                        let (value, cut) = cut(value, limit);
                        if cut {
                            truncated = Err(TraceOmission::ValueTruncated);
                        }
                        value
                    })
                    .collect(),
            ),
            value => value,
        };
        Ok((value, truncated))
    }
}

fn key_len(key: &AttrKey) -> u64 {
    u64::try_from(key.as_str().len()).unwrap_or(u64::MAX)
}

/// `value` cut at the last UTF-8 boundary at or below `limit` bytes, and
/// whether it was cut.
fn cut(value: Cow<'static, str>, limit: usize) -> (Cow<'static, str>, bool) {
    if value.len() <= limit {
        return (value, false);
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (Cow::Owned(value[..end].to_owned()), true)
}
