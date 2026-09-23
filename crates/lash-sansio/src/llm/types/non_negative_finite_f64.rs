use schemars::JsonSchema;

/// Largest integer magnitude binary64 holds exactly. Above it, consecutive
/// integers share a representation, so reading the value back as `f64` would
/// silently change it.
const MAX_EXACT_INTEGER: u64 = 1 << 53;

/// A sampling number that is safe to place in a provider request body: finite
/// (never NaN or ±infinity), never negative, and exactly representable as a
/// binary64 float.
///
/// Backed by [`serde_json::Number`] rather than `f64` for two reasons. It keeps
/// [`GenerationOptions`](super::GenerationOptions) — and therefore every
/// request, durable envelope and protocol type that carries it — `Eq`, which a
/// bare `f64` would destroy. And it makes the invariant structural: a value
/// that cannot be encoded as JSON can never be constructed, so no adapter has
/// to re-check before building a `json!` body.
///
/// The JSON number a caller wrote is preserved, not re-spelled: an integer `1`
/// decoded from a wire payload stays the integer `1` and re-encodes as `1`, so
/// a protocol round trip is exact in both directions. The only representations
/// refused are the ones binary64 cannot hold — integers above 2<sup>53</sup> —
/// which keeps [`get`](Self::get) exact for every value that can exist.
/// Equality is numeric rather than textual: `1` and `1.0` are the same sampling
/// number.
///
/// Endpoint- and model-specific upper bounds (OpenAI caps temperature at 2,
/// Anthropic at 1) stay in the adapters, which are the only layer that knows
/// them.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "serde_json::Number", into = "serde_json::Number")]
pub struct NonNegativeFiniteF64(serde_json::Number);

/// The serde layer accepts any JSON number and `try_from` then refuses
/// negative or non-finite values, so the decode shape is a number with a
/// zero lower bound.
impl JsonSchema for NonNegativeFiniteF64 {
    fn schema_name() -> String {
        "NonNegativeFiniteF64".to_string()
    }
    fn json_schema(_: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        schemars::schema::Schema::Object(schemars::schema::SchemaObject {
            instance_type: Some(schemars::schema::InstanceType::Number.into()),
            number: Some(Box::new(schemars::schema::NumberValidation {
                minimum: Some(0.0),
                ..Default::default()
            })),
            ..Default::default()
        })
    }
}

impl NonNegativeFiniteF64 {
    pub fn new(value: f64) -> Result<Self, NonNegativeFiniteF64Error> {
        Self::check_finite_non_negative(value)?;
        // `-0.0` passes the sign check but would serialize as `-0.0`, which
        // reads as a negative number on the wire. Normalize it away.
        let value = if value == 0.0 { 0.0 } else { value };
        serde_json::Number::from_f64(value)
            .map(Self)
            .ok_or_else(|| NonNegativeFiniteF64Error {
                message: format!("{value} is not representable as a JSON number"),
            })
    }

    fn check_finite_non_negative(value: f64) -> Result<(), NonNegativeFiniteF64Error> {
        if !value.is_finite() {
            return Err(NonNegativeFiniteF64Error {
                message: format!("expected a finite number, got {value}"),
            });
        }
        // `-0.0 < 0.0` is false, so negative zero is accepted here and
        // normalized by the caller rather than rejected.
        if value < 0.0 {
            return Err(NonNegativeFiniteF64Error {
                message: format!("expected a non-negative number, got {value}"),
            });
        }
        Ok(())
    }

    #[expect(
        clippy::expect_used,
        reason = "every constructor runs `validate`, which refuses anything but a finite JSON number"
    )]
    pub fn get(&self) -> f64 {
        self.0
            .as_f64()
            .expect("NonNegativeFiniteF64 only ever holds a finite JSON number")
    }
}

/// Numeric, not textual: two sampling numbers of the same value are equal even
/// when they were written with different JSON spellings.
impl PartialEq for NonNegativeFiniteF64 {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

/// Total because the invariant excludes NaN.
impl Eq for NonNegativeFiniteF64 {}

impl std::fmt::Display for NonNegativeFiniteF64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl From<NonNegativeFiniteF64> for serde_json::Number {
    fn from(value: NonNegativeFiniteF64) -> Self {
        value.0
    }
}

impl TryFrom<serde_json::Number> for NonNegativeFiniteF64 {
    type Error = NonNegativeFiniteF64Error;

    /// The accepted `Number` is carried through unchanged, so no decode re-spells or rounds
    /// what the sender wrote.
    fn try_from(value: serde_json::Number) -> Result<Self, Self::Error> {
        let as_f64 = value.as_f64().ok_or_else(|| NonNegativeFiniteF64Error {
            message: format!("{value} is not representable as a finite number"),
        })?;
        Self::check_finite_non_negative(as_f64)?;
        if value
            .as_u64()
            .is_some_and(|integer| integer > MAX_EXACT_INTEGER)
        {
            return Err(NonNegativeFiniteF64Error {
                message: format!("{value} cannot be represented exactly as a finite number"),
            });
        }
        if as_f64 == 0.0 && as_f64.is_sign_negative() {
            return Self::new(0.0);
        }
        Ok(Self(value))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NonNegativeFiniteF64Error {
    pub message: String,
}

impl std::fmt::Display for NonNegativeFiniteF64Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for NonNegativeFiniteF64Error {}
