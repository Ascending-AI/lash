//! Persistent semantic heap pages and the admitted requests resumed workers receive.
pub mod paged;
use lash_kernel_doc::graph_tokens::{TokenCodec, TokenError, TokenReader, TokenWriter};
use lash_kernel_doc::{Datum, EffectIdentity, EffectName};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// One wait a machine hands out, numbered in request order.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct WaitId(pub u64);
/// A sleep's length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Pause {
    pub seconds: u64,
    /// Below one second.
    pub nanoseconds: u32,
}

impl From<Duration> for Pause {
    fn from(duration: Duration) -> Self {
        Self {
            seconds: duration.as_secs(),
            nanoseconds: duration.subsec_nanos(),
        }
    }
}

impl Pause {
    /// The duration, or `None` when the nanoseconds reach a second.
    pub fn duration(self) -> Option<Duration> {
        (self.nanoseconds < 1_000_000_000).then(|| Duration::new(self.seconds, self.nanoseconds))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AdmittedRequest {
    Effect {
        effect: EffectName,
        args: Vec<Datum>,
    },
    Sleep {
        duration: Pause,
    },
}

/// One admitted wait, keyed by its effect identity: what a parent hands a
/// worker beside a checkpoint root to resume it.
#[derive(Clone, Debug, PartialEq)]
pub struct AdmittedWait {
    pub identity: EffectIdentity,
    pub request: AdmittedRequest,
}

fn unsupported(variant: u32) -> TokenError {
    TokenError::UnsupportedVariant { variant }
}

fn invalid(problem: &str) -> TokenError {
    TokenError::Invalid(problem.to_owned())
}

/// Reads a sum header and checks the variant's field count with `arity`.
pub(crate) fn variant(
    input: &mut TokenReader<'_>,
    arity: impl FnOnce(u32) -> Option<u64>,
) -> Result<u32, TokenError> {
    let (variant, fields) = input.sum()?;
    let expected = arity(variant).ok_or_else(|| unsupported(variant))?;
    TokenReader::expect_fields(fields, expected)?;
    Ok(variant)
}

impl TokenCodec for WaitId {
    fn encode(&self, out: &mut TokenWriter) {
        out.unsigned(self.0);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.unsigned().map(Self)
    }
}

impl TokenCodec for Pause {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        out.unsigned(self.seconds);
        out.unsigned(u64::from(self.nanoseconds));
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        let pause = Self {
            seconds: input.unsigned()?,
            nanoseconds: input.unsigned_u32()?,
        };
        pause
            .duration()
            .map(|_| pause)
            .ok_or_else(|| invalid("a pause's nanoseconds reach a second"))
    }
}

impl TokenCodec for AdmittedRequest {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Effect { effect, args } => {
                out.sum(0, 2);
                effect.encode(out);
                args.encode(out);
            }
            Self::Sleep { duration } => {
                out.sum(1, 1);
                duration.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| match variant {
                0 => Some(2),
                1 => Some(1),
                _ => None,
            })? {
                0 => Self::Effect {
                    effect: EffectName::decode(input)?,
                    args: Vec::decode(input)?,
                },
                _ => Self::Sleep {
                    duration: Pause::decode(input)?,
                },
            },
        )
    }
}

impl TokenCodec for AdmittedWait {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.identity.encode(out);
        self.request.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            identity: EffectIdentity::decode(input)?,
            request: AdmittedRequest::decode(input)?,
        })
    }
}
