//! Decoding an inbound datum by a type (`K-EFF-005` to `K-EFF-007`): the
//! one rule a run reads an effect's result by, and a host admits a result
//! by before it reaches a run.

use num_bigint::BigInt;
use num_traits::{FromPrimitive, ToPrimitive, Zero};

use crate::{Datum, ErrorDatum, Float, Integer, NumberPolicy, NumberToken};
use lash_vocabulary::types::Type;

/// How an inbound datum is read.
pub struct Decoder<'a> {
    /// The deepest a decoded value nests.
    pub max_depth: usize,
    /// How a bare number decodes.
    pub policy: NumberPolicy,
    /// Whether the document declares a function of this name.
    pub declared: &'a dyn Fn(&lash_vocabulary::names::CallableId) -> bool,
}

/// Decoding refuses a value mismatch separately from a configured depth limit
/// or an unavailable callable identity.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
    thiserror::Error,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DecodeRefusal {
    #[error("{mismatch}")]
    Type {
        mismatch: lash_vocabulary::TypeMismatch,
    },
    #[error("value exceeds the configured {limit}-level depth")]
    DepthLimit { limit: usize },
    #[error("the callable is not declared: {callable:?}")]
    UndeclaredFunction {
        callable: lash_vocabulary::CallableId,
    },
}
impl DecodeRefusal {
    pub fn mismatch(ty: &Type, datum: &Datum) -> Self {
        Self::Type {
            mismatch: lash_vocabulary::TypeMismatch::new(ty.clone(), datum_type(datum)),
        }
    }
    pub fn within(self, segment: &str) -> Self {
        match self {
            Self::Type { mismatch } => Self::Type {
                mismatch: mismatch.within(segment),
            },
            other => other,
        }
    }
    fn in_map(self, key: &Datum, index: usize, part: &str) -> Self {
        match key {
            Datum::Text(name) => self.within(name),
            _ => self.within(part).within(&index.to_string()),
        }
    }
}

/// A shallow kind witness, bounded independently of a value's contents.
pub fn datum_type(datum: &Datum) -> Type {
    match datum {
        Datum::Null => Type::Null,
        Datum::Absent => Type::Absent,
        Datum::Bool(_) => Type::Bool,
        Datum::Int(_) => Type::Int,
        Datum::Float(_) => Type::Float,
        Datum::Number(_) => Type::Number,
        Datum::Text(_) => Type::Text,
        Datum::Bytes(_) => Type::Bytes,
        Datum::Timestamp(_) => Type::Timestamp,
        Datum::Tuple(items) => Type::Tuple(vec![Type::Any; items.len().min(64)]),
        Datum::List(_) => Type::List(Box::new(Type::Any)),
        Datum::Map(_) => Type::Map(Box::new(lash_vocabulary::MapType {
            key: Type::Any,
            value: Type::Any,
        })),
        Datum::Set(_) => Type::Set(Box::new(Type::Any)),
        Datum::Record(_) => Type::Record(lash_vocabulary::RecordType {
            fields: vec![],
            rest: Some(Box::new(Type::Any)),
        }),
        Datum::Error(_) => Type::Error,
        Datum::Function(_) | Datum::FunctionValue(_) => {
            Type::Function(Box::new(lash_vocabulary::Signature {
                params: vec![],
                result: Type::Any,
            }))
        }
        Datum::Handle(handle) => Type::Handle(handle.kind.clone()),
    }
}

type Fit = Result<Datum, DecodeRefusal>;

/// The exact integer a JSON number spells, when its value is integral.
fn integral(token: &NumberToken) -> Option<BigInt> {
    let text = token.as_str();
    let (mantissa, exponent) = match text.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i64>().ok()?),
        None => (text, 0),
    };
    let (negative, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, mantissa),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let mut digits = format!("{whole}{fraction}");
    let mut exponent = exponent.checked_sub(i64::try_from(fraction.len()).ok()?)?;
    while exponent < 0 && digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
        exponent += 1;
    }
    let value: BigInt = digits.parse().ok()?;
    if value.is_zero() {
        return Some(value);
    }
    // A number this large is not one a run can hold.
    if !(0..=4096).contains(&exponent) {
        return None;
    }
    let value = value * BigInt::from(10u8).pow(u32::try_from(exponent).ok()?);
    Some(if negative { -value } else { value })
}

fn finite(value: f64, datum: &Datum, ty: &Type) -> Fit {
    if value.is_finite() {
        Ok(Datum::Float(Float::new(value)))
    } else {
        Err(DecodeRefusal::mismatch(ty, datum))
    }
}
fn nearest(token: &NumberToken) -> Fit {
    let datum = Datum::Number(token.clone());
    let value: f64 = token
        .as_str()
        .parse()
        .map_err(|_| DecodeRefusal::mismatch(&Type::Float, &datum))?;
    finite(value, &datum, &Type::Float)
}

fn whole(float: f64) -> Option<Datum> {
    (float.is_finite() && float.fract() == 0.0)
        .then(|| BigInt::from_f64(float))
        .flatten()
        .map(|integer| Datum::Int(Integer::new(integer)))
}

impl Decoder<'_> {
    /// Reads `datum` as `ty`: the same tree with every number decoded and
    /// every part in the shape the type names, or why it does not fit.
    ///
    /// # Errors
    ///
    /// [`DecodeRefusal`]: the first part of `datum` that `ty` does not
    /// admit, or a value nested deeper than [`Self::max_depth`].
    pub fn decode(&self, datum: &Datum, ty: &Type) -> Fit {
        self.fit(datum, ty, self.max_depth)
    }

    fn all(&self, items: &[Datum], ty: &Type, budget: usize) -> Result<Vec<Datum>, DecodeRefusal> {
        items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                self.fit(item, ty, budget)
                    .map_err(|e| e.within(&index.to_string()))
            })
            .collect()
    }

    fn too_deep(&self) -> DecodeRefusal {
        DecodeRefusal::DepthLimit {
            limit: self.max_depth,
        }
    }

    fn fit(&self, datum: &Datum, ty: &Type, budget: usize) -> Fit {
        let Some(budget) = budget.checked_sub(1) else {
            return Err(self.too_deep());
        };
        let mismatch = || DecodeRefusal::mismatch(ty, datum);
        match (ty, datum) {
            (Type::Union(members), _) => {
                for member in members {
                    match self.fit(datum, member, budget) {
                        Ok(value) => return Ok(value),
                        Err(DecodeRefusal::Type { .. }) => {}
                        Err(other) => return Err(other),
                    }
                }
                Err(mismatch())
            }
            (Type::Any, _) => self.any(datum, budget),
            (Type::Json, _) => self.json(datum, budget),
            (Type::Null, Datum::Null)
            | (Type::Absent, Datum::Absent)
            | (Type::Bool, Datum::Bool(_))
            | (Type::Text, Datum::Text(_))
            | (Type::Bytes, Datum::Bytes(_))
            | (Type::Timestamp, Datum::Timestamp(_))
            | (Type::Int, Datum::Int(_))
            | (Type::Float, Datum::Float(_))
            | (Type::Number, Datum::Int(_) | Datum::Float(_)) => Ok(datum.clone()),
            (Type::Int, Datum::Float(float)) => {
                whole(float.get()).ok_or_else(|| DecodeRefusal::mismatch(ty, datum))
            }
            (Type::Int, Datum::Number(token)) => integral(token)
                .map(|integer| Datum::Int(Integer::new(integer)))
                .ok_or_else(|| DecodeRefusal::mismatch(ty, datum)),
            (Type::Float, Datum::Int(integer)) => {
                finite(integer.to_f64().unwrap_or(f64::INFINITY), datum, ty)
            }
            (Type::Float, Datum::Number(token)) => nearest(token),
            (Type::Number, Datum::Number(token)) => self.bare(token),
            (Type::Error, Datum::Error(error)) => Ok(Datum::Error(Box::new(ErrorDatum {
                kind: error.kind.clone(),
                message: error.message.clone(),
                data: self
                    .any(&error.data, budget)
                    .map_err(|e| e.within("data"))?,
            }))),
            (Type::Enum(members), Datum::Text(text)) => {
                if members.contains(text) {
                    Ok(datum.clone())
                } else {
                    Err(DecodeRefusal::mismatch(ty, datum))
                }
            }
            (Type::Handle(kind), Datum::Handle(handle)) => {
                if handle.kind == *kind {
                    Ok(datum.clone())
                } else {
                    Err(DecodeRefusal::mismatch(ty, datum))
                }
            }
            (Type::Function(_), Datum::Function(name)) => self.function(name, datum),
            (Type::Tuple(types), Datum::Tuple(items) | Datum::List(items)) => {
                if types.len() != items.len() {
                    return Err(DecodeRefusal::mismatch(ty, datum));
                }
                Ok(Datum::Tuple(
                    items
                        .iter()
                        .zip(types)
                        .enumerate()
                        .map(|(index, (item, ty))| {
                            self.fit(item, ty, budget)
                                .map_err(|e| e.within(&index.to_string()))
                        })
                        .collect::<Result<_, _>>()?,
                ))
            }
            (Type::List(item), Datum::List(items)) => {
                Ok(Datum::List(self.all(items, item, budget)?))
            }
            (Type::Set(item), Datum::Set(items) | Datum::List(items)) => {
                Ok(Datum::Set(self.all(items, item, budget)?))
            }
            (Type::Map(map), Datum::Map(entries)) => Ok(Datum::Map(
                entries
                    .iter()
                    .enumerate()
                    .map(|(index, (key, value))| {
                        Ok((
                            self.fit(key, &map.key, budget)
                                .map_err(|e| e.in_map(key, index, "0"))?,
                            self.fit(value, &map.value, budget)
                                .map_err(|e| e.in_map(key, index, "1"))?,
                        ))
                    })
                    .collect::<Result<_, DecodeRefusal>>()?,
            )),
            // A JSON object read as a map: its member names are the keys.
            (Type::Map(map), Datum::Record(fields)) => Ok(Datum::Map(
                fields
                    .iter()
                    .map(|(name, value)| {
                        Ok((
                            self.fit(&Datum::Text(name.clone()), &map.key, budget)
                                .map_err(|e| e.within(name))?,
                            self.fit(value, &map.value, budget)
                                .map_err(|e| e.within(name))?,
                        ))
                    })
                    .collect::<Result<_, DecodeRefusal>>()?,
            )),
            (Type::Record(record), Datum::Record(fields)) => {
                for field in &record.fields {
                    if !field.optional && !fields.iter().any(|(name, _)| *name == field.name) {
                        return Err(
                            DecodeRefusal::mismatch(&field.ty, &Datum::Absent).within(&field.name)
                        );
                    }
                }
                let mut decoded = Vec::with_capacity(fields.len());
                for (name, value) in fields {
                    let declared = record.fields.iter().find(|field| field.name == *name);
                    let ty = match (declared, &record.rest) {
                        (Some(field), _) => &field.ty,
                        (None, Some(rest)) => rest.as_ref(),
                        (None, None) => {
                            return Err(DecodeRefusal::mismatch(&Type::Absent, value).within(name));
                        }
                    };
                    let value = self
                        .fit(value, ty, budget)
                        .map_err(|mismatch| mismatch.within(name))?;
                    decoded.push((name.clone(), value));
                }
                Ok(Datum::Record(decoded))
            }
            _ => Err(mismatch()),
        }
    }

    /// Reads `datum` as [`Type::Json`]: as [`Self::any`] does, refusing
    /// a function reference or a handle at any depth.
    fn json(&self, datum: &Datum, budget: usize) -> Fit {
        let Some(budget) = budget.checked_sub(1) else {
            return Err(self.too_deep());
        };
        let all = |items: &[Datum]| -> Result<Vec<Datum>, DecodeRefusal> {
            items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    self.json(item, budget)
                        .map_err(|e| e.within(&index.to_string()))
                })
                .collect()
        };
        Ok(match datum {
            Datum::Number(token) => self.bare(token)?,
            Datum::Function(_) | Datum::Handle(_) | Datum::FunctionValue(_) => {
                return Err(DecodeRefusal::mismatch(&Type::Json, datum));
            }
            Datum::Tuple(items) => Datum::Tuple(all(items)?),
            Datum::List(items) => Datum::List(all(items)?),
            Datum::Set(items) => Datum::Set(all(items)?),
            Datum::Map(entries) => Datum::Map(
                entries
                    .iter()
                    .enumerate()
                    .map(|(index, (key, value))| {
                        Ok((
                            self.json(key, budget)
                                .map_err(|e| e.in_map(key, index, "0"))?,
                            self.json(value, budget)
                                .map_err(|e| e.in_map(key, index, "1"))?,
                        ))
                    })
                    .collect::<Result<_, DecodeRefusal>>()?,
            ),
            Datum::Record(fields) => Datum::Record(
                fields
                    .iter()
                    .map(|(name, value)| {
                        Ok((
                            name.clone(),
                            self.json(value, budget)
                                .map_err(|mismatch| mismatch.within(name))?,
                        ))
                    })
                    .collect::<Result<_, DecodeRefusal>>()?,
            ),
            Datum::Error(error) => Datum::Error(Box::new(ErrorDatum {
                kind: error.kind.clone(),
                message: error.message.clone(),
                data: self
                    .json(&error.data, budget)
                    .map_err(|e| e.within("data"))?,
            })),
            other => other.clone(),
        })
    }

    fn bare(&self, token: &NumberToken) -> Fit {
        match self.policy {
            NumberPolicy::BySpelling if token.is_integer_spelling() => integral(token)
                .map(|integer| Datum::Int(Integer::new(integer)))
                .ok_or_else(|| DecodeRefusal::mismatch(&Type::Int, &Datum::Number(token.clone()))),
            _ => nearest(token),
        }
    }

    fn function(&self, name: &lash_vocabulary::names::CallableId, datum: &Datum) -> Fit {
        if (self.declared)(name) {
            Ok(datum.clone())
        } else {
            Err(DecodeRefusal::UndeclaredFunction { callable: *name })
        }
    }

    fn any(&self, datum: &Datum, budget: usize) -> Fit {
        let Some(budget) = budget.checked_sub(1) else {
            return Err(self.too_deep());
        };
        let all = |items: &[Datum]| -> Result<Vec<Datum>, DecodeRefusal> {
            items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    self.any(item, budget)
                        .map_err(|e| e.within(&index.to_string()))
                })
                .collect()
        };
        Ok(match datum {
            Datum::Number(token) => self.bare(token)?,
            Datum::Function(name) => self.function(name, datum)?,
            Datum::FunctionValue(_) => {
                return Err(DecodeRefusal::mismatch(&Type::Any, datum));
            }
            Datum::Tuple(items) => Datum::Tuple(all(items)?),
            Datum::List(items) => Datum::List(all(items)?),
            Datum::Set(items) => Datum::Set(all(items)?),
            Datum::Map(entries) => Datum::Map(
                entries
                    .iter()
                    .enumerate()
                    .map(|(index, (key, value))| {
                        Ok((
                            self.any(key, budget)
                                .map_err(|e| e.in_map(key, index, "0"))?,
                            self.any(value, budget)
                                .map_err(|e| e.in_map(key, index, "1"))?,
                        ))
                    })
                    .collect::<Result<_, DecodeRefusal>>()?,
            ),
            Datum::Record(fields) => Datum::Record(
                fields
                    .iter()
                    .map(|(name, value)| {
                        Ok((
                            name.clone(),
                            self.any(value, budget).map_err(|e| e.within(name))?,
                        ))
                    })
                    .collect::<Result<_, DecodeRefusal>>()?,
            ),
            Datum::Error(error) => Datum::Error(Box::new(ErrorDatum {
                kind: error.kind.clone(),
                message: error.message.clone(),
                data: self
                    .any(&error.data, budget)
                    .map_err(|e| e.within("data"))?,
            })),
            other => other.clone(),
        })
    }
}

/// Encode a typed diagnostic as ordinary data, preserving its structure when
/// an error crosses the guest/host boundary.
pub fn diagnostic_datum(value: &impl serde::Serialize) -> Result<Datum, crate::EncodeError> {
    fn datum(value: serde_json::Value) -> Result<Datum, crate::EncodeError> {
        Ok(match value {
            serde_json::Value::Null => Datum::Null,
            serde_json::Value::Bool(v) => Datum::Bool(v),
            serde_json::Value::Number(v) => {
                if let Some(v) = v.as_i64() {
                    Datum::Int(Integer::from(v))
                } else if let Some(v) = v.as_u64() {
                    Datum::Int(Integer::new(BigInt::from(v)))
                } else {
                    Datum::Float(Float::new(v.as_f64().ok_or_else(|| {
                        crate::EncodeError {
                            message: "diagnostic number is not representable".into(),
                        }
                    })?))
                }
            }
            serde_json::Value::String(v) => Datum::Text(v),
            serde_json::Value::Array(v) => {
                Datum::List(v.into_iter().map(datum).collect::<Result<_, _>>()?)
            }
            serde_json::Value::Object(v) => Datum::Record(
                v.into_iter()
                    .map(|(k, v)| Ok((k, datum(v)?)))
                    .collect::<Result<_, crate::EncodeError>>()?,
            ),
        })
    }
    datum(serde_json::to_value(value).map_err(|e| crate::EncodeError {
        message: e.to_string(),
    })?)
}

impl DecodeRefusal {
    /// The same typed failure for an immediate read or a committed effect answer.
    pub fn error_datum(&self) -> Result<ErrorDatum, crate::EncodeError> {
        let (kind, data) = match self {
            Self::Type { mismatch } => ("effect_result", diagnostic_datum(mismatch)?),
            _ => ("decode_refused", diagnostic_datum(self)?),
        };
        Ok(ErrorDatum {
            kind: kind.into(),
            message: kind.into(),
            data,
        })
    }
}
