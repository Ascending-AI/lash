//! Decoding an inbound datum by a type (`K-EFF-005` to `K-EFF-007`): the
//! one rule a run reads an effect's result by, and a host admits a result
//! by before it reaches a run.

use num_bigint::BigInt;
use num_traits::{FromPrimitive, ToPrimitive, Zero};

use crate::{Datum, ErrorDatum, Float, Integer, Name, NumberPolicy, NumberToken, Type};

/// How an inbound datum is read.
pub struct Decoder<'a> {
    /// The deepest a decoded value nests.
    pub max_depth: usize,
    /// How a bare number decodes.
    pub policy: NumberPolicy,
    /// Whether the document declares a function of this name.
    pub declared: &'a dyn Fn(&Name) -> bool,
}

/// Why a datum does not fit a type: the problem, at the record fields that
/// lead to it from the root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeMismatch {
    /// The record fields from the root to the part that does not fit,
    /// outermost first; empty when the root does not fit.
    pub path: Vec<String>,
    /// What does not fit there.
    pub problem: String,
}

impl DecodeMismatch {
    fn new(problem: impl Into<String>) -> Self {
        Self {
            path: Vec::new(),
            problem: problem.into(),
        }
    }

    fn within(mut self, field: &str) -> Self {
        self.path.insert(0, field.to_owned());
        self
    }
}

impl std::fmt::Display for DecodeMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for field in &self.path {
            write!(f, "field `{field}`: ")?;
        }
        f.write_str(&self.problem)
    }
}

impl std::error::Error for DecodeMismatch {}

type Fit = Result<Datum, DecodeMismatch>;

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

fn finite(value: f64, what: &str) -> Fit {
    if value.is_finite() {
        Ok(Datum::Float(Float::new(value)))
    } else {
        Err(DecodeMismatch::new(format!(
            "{what} has no finite nearest float"
        )))
    }
}

fn nearest(token: &NumberToken) -> Fit {
    let value: f64 = token
        .as_str()
        .parse()
        .map_err(|_| DecodeMismatch::new(format!("`{}` is not a number", token.as_str())))?;
    finite(value, token.as_str())
}

fn whole(float: f64) -> Option<Datum> {
    (float.is_finite() && float.fract() == 0.0)
        .then(|| BigInt::from_f64(float))
        .flatten()
        .map(|integer| Datum::Int(Integer::new(integer)))
}

fn kind(datum: &Datum) -> &'static str {
    match datum {
        Datum::Null => "null",
        Datum::Absent => "absent",
        Datum::Bool(_) => "a bool",
        Datum::Int(_) | Datum::Float(_) | Datum::Number(_) => "a number",
        Datum::Text(_) => "a text",
        Datum::Bytes(_) => "bytes",
        Datum::Timestamp(_) => "a timestamp",
        Datum::Tuple(_) => "a tuple",
        Datum::List(_) => "a list",
        Datum::Map(_) => "a map",
        Datum::Set(_) => "a set",
        Datum::Record(_) => "a record",
        Datum::Error(_) => "an error",
        Datum::Function(_) => "a function reference",
        Datum::Handle(_) => "a handle",
    }
}

impl Decoder<'_> {
    /// Reads `datum` as `ty`: the same tree with every number decoded and
    /// every part in the shape the type names, or why it does not fit.
    ///
    /// # Errors
    ///
    /// [`DecodeMismatch`]: the first part of `datum` that `ty` does not
    /// admit, or a value nested deeper than [`Self::max_depth`].
    pub fn decode(&self, datum: &Datum, ty: &Type) -> Fit {
        self.fit(datum, ty, self.max_depth)
    }

    fn all(&self, items: &[Datum], ty: &Type, budget: usize) -> Result<Vec<Datum>, DecodeMismatch> {
        items
            .iter()
            .map(|item| self.fit(item, ty, budget))
            .collect()
    }

    fn too_deep(&self) -> DecodeMismatch {
        DecodeMismatch::new(format!(
            "it exceeds the configured {}-level value depth",
            self.max_depth
        ))
    }

    fn fit(&self, datum: &Datum, ty: &Type, budget: usize) -> Fit {
        let Some(budget) = budget.checked_sub(1) else {
            return Err(self.too_deep());
        };
        let mismatch = |expected: &str| {
            Err(DecodeMismatch::new(format!(
                "expected {expected}, found {}",
                kind(datum)
            )))
        };
        match (ty, datum) {
            (Type::Union(members), _) => members
                .iter()
                .find_map(|member| self.fit(datum, member, budget).ok())
                .ok_or_else(|| {
                    DecodeMismatch::new(format!("{} fits no member of the union", kind(datum)))
                }),
            (Type::Any, _) => self.any(datum, budget),
            (Type::Null, Datum::Null)
            | (Type::Absent, Datum::Absent)
            | (Type::Bool, Datum::Bool(_))
            | (Type::Text, Datum::Text(_))
            | (Type::Bytes, Datum::Bytes(_))
            | (Type::Timestamp, Datum::Timestamp(_))
            | (Type::Int, Datum::Int(_))
            | (Type::Float, Datum::Float(_))
            | (Type::Number, Datum::Int(_) | Datum::Float(_)) => Ok(datum.clone()),
            (Type::Int, Datum::Float(float)) => whole(float.get())
                .ok_or_else(|| DecodeMismatch::new(format!("{float} is not an integer"))),
            (Type::Int, Datum::Number(token)) => integral(token)
                .map(|integer| Datum::Int(Integer::new(integer)))
                .ok_or_else(|| {
                    DecodeMismatch::new(format!("{} is not an integer", token.as_str()))
                }),
            (Type::Float, Datum::Int(integer)) => {
                finite(integer.to_f64().unwrap_or(f64::INFINITY), "the integer")
            }
            (Type::Float, Datum::Number(token)) => nearest(token),
            (Type::Number, Datum::Number(token)) => self.bare(token),
            (Type::Error, Datum::Error(error)) => Ok(Datum::Error(Box::new(ErrorDatum {
                kind: error.kind.clone(),
                message: error.message.clone(),
                data: self.any(&error.data, budget)?,
            }))),
            (Type::Enum(members), Datum::Text(text)) => {
                if members.contains(text) {
                    Ok(datum.clone())
                } else {
                    Err(DecodeMismatch::new(format!(
                        "`{text}` is not a member of the enum"
                    )))
                }
            }
            (Type::Handle(kind), Datum::Handle(handle)) => {
                if handle.kind == *kind {
                    Ok(datum.clone())
                } else {
                    Err(DecodeMismatch::new(format!(
                        "expected a `{kind}` handle, found a `{}` handle",
                        handle.kind
                    )))
                }
            }
            (Type::Function(_), Datum::Function(name)) => self.function(name, datum),
            (Type::Tuple(types), Datum::Tuple(items) | Datum::List(items)) => {
                if types.len() != items.len() {
                    return Err(DecodeMismatch::new(format!(
                        "expected a tuple of {}, found {} member(s)",
                        types.len(),
                        items.len()
                    )));
                }
                Ok(Datum::Tuple(
                    items
                        .iter()
                        .zip(types)
                        .map(|(item, ty)| self.fit(item, ty, budget))
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
                    .map(|(key, value)| {
                        Ok((
                            self.fit(key, &map.key, budget)?,
                            self.fit(value, &map.value, budget)?,
                        ))
                    })
                    .collect::<Result<_, DecodeMismatch>>()?,
            )),
            // A JSON object read as a map: its member names are the keys.
            (Type::Map(map), Datum::Record(fields)) => Ok(Datum::Map(
                fields
                    .iter()
                    .map(|(name, value)| {
                        Ok((
                            self.fit(&Datum::Text(name.clone()), &map.key, budget)?,
                            self.fit(value, &map.value, budget)?,
                        ))
                    })
                    .collect::<Result<_, DecodeMismatch>>()?,
            )),
            (Type::Record(record), Datum::Record(fields)) => {
                for field in &record.fields {
                    if !field.optional && !fields.iter().any(|(name, _)| *name == field.name) {
                        return Err(DecodeMismatch::new(format!(
                            "the required field `{}` is missing",
                            field.name
                        )));
                    }
                }
                let mut decoded = Vec::with_capacity(fields.len());
                for (name, value) in fields {
                    let declared = record.fields.iter().find(|field| field.name == *name);
                    let ty = match (declared, &record.rest) {
                        (Some(field), _) => &field.ty,
                        (None, Some(rest)) => rest.as_ref(),
                        (None, None) => {
                            return Err(DecodeMismatch::new(format!(
                                "the record type has no field `{name}`"
                            )));
                        }
                    };
                    let value = self
                        .fit(value, ty, budget)
                        .map_err(|mismatch| mismatch.within(name))?;
                    decoded.push((name.clone(), value));
                }
                Ok(Datum::Record(decoded))
            }
            (Type::Null, _) => mismatch("null"),
            (Type::Absent, _) => mismatch("absent"),
            (Type::Bool, _) => mismatch("a bool"),
            (Type::Int, _) => mismatch("an integer"),
            (Type::Float, _) => mismatch("a float"),
            (Type::Number, _) => mismatch("a number"),
            (Type::Text | Type::Enum(_), _) => mismatch("a text"),
            (Type::Bytes, _) => mismatch("bytes"),
            (Type::Timestamp, _) => mismatch("a timestamp"),
            (Type::Tuple(_), _) => mismatch("a tuple"),
            (Type::List(_), _) => mismatch("a list"),
            (Type::Map(_), _) => mismatch("a map"),
            (Type::Set(_), _) => mismatch("a set"),
            (Type::Record(_), _) => mismatch("a record"),
            (Type::Function(_), _) => mismatch("a function reference"),
            (Type::Task(_), _) => mismatch("a task handle, which no effect returns"),
            (Type::Error, _) => mismatch("an error"),
            (Type::Handle(_), _) => mismatch("a handle"),
        }
    }

    fn bare(&self, token: &NumberToken) -> Fit {
        match self.policy {
            NumberPolicy::BySpelling if token.is_integer_spelling() => integral(token)
                .map(|integer| Datum::Int(Integer::new(integer)))
                .ok_or_else(|| {
                    DecodeMismatch::new(format!("{} is not an integer", token.as_str()))
                }),
            _ => nearest(token),
        }
    }

    fn function(&self, name: &Name, datum: &Datum) -> Fit {
        if (self.declared)(name) {
            Ok(datum.clone())
        } else {
            Err(DecodeMismatch::new(format!(
                "the document declares no function `{name}`"
            )))
        }
    }

    fn any(&self, datum: &Datum, budget: usize) -> Fit {
        let Some(budget) = budget.checked_sub(1) else {
            return Err(self.too_deep());
        };
        let all = |items: &[Datum]| -> Result<Vec<Datum>, DecodeMismatch> {
            items.iter().map(|item| self.any(item, budget)).collect()
        };
        Ok(match datum {
            Datum::Number(token) => self.bare(token)?,
            Datum::Function(name) => self.function(name, datum)?,
            Datum::Tuple(items) => Datum::Tuple(all(items)?),
            Datum::List(items) => Datum::List(all(items)?),
            Datum::Set(items) => Datum::Set(all(items)?),
            Datum::Map(entries) => Datum::Map(
                entries
                    .iter()
                    .map(|(key, value)| Ok((self.any(key, budget)?, self.any(value, budget)?)))
                    .collect::<Result<_, DecodeMismatch>>()?,
            ),
            Datum::Record(fields) => Datum::Record(
                fields
                    .iter()
                    .map(|(name, value)| Ok((name.clone(), self.any(value, budget)?)))
                    .collect::<Result<_, DecodeMismatch>>()?,
            ),
            Datum::Error(error) => Datum::Error(Box::new(ErrorDatum {
                kind: error.kind.clone(),
                message: error.message.clone(),
                data: self.any(&error.data, budget)?,
            })),
            other => other.clone(),
        })
    }
}
