//! The arena a run executes on, the persistent semantic heap pages it is
//! projected to, and the admitted requests resumed workers receive.
pub mod arena;
pub mod paged;
#[cfg(any(test, feature = "perf-witness"))]
pub(crate) mod perf_witness;
/// Semantic state readers for carry and continuation equality.
pub mod semantic;

/// Page-level internals, for the other kernel crates only: the machine
/// seals and reads through them and kernel laws inspect them. Nothing outside
/// `crates/lash-kernel-*` names this module (`scripts/check-kernel-boundary.py`,
/// FIG-6688); hosts use [`paged`]'s records, page port and session readers.
#[doc(hidden)]
pub mod kernel_internals {
    pub fn checkpoint(root: crate::paged::CheckpointRoot) -> crate::paged::Checkpoint {
        crate::paged::checkpoint::Checkpoint(root, None)
    }
    pub fn checkpoint_root(checkpoint: &crate::paged::Checkpoint) -> &crate::paged::CheckpointRoot {
        &checkpoint.0
    }

    #[cfg(any(test, feature = "perf-witness"))]
    pub mod perf_witness {
        pub use crate::perf_witness::*;
    }
    pub mod tree {
        pub use crate::paged::tree::*;
    }
}
use lash_kernel_doc::Datum;
use lash_kernel_value::names::OperationUse;
use lash_kernel_value::sites::EffectId;
use lash_kernel_value::tokens::TokenCodec;
use lash_kernel_value::tokens::TokenError;
use lash_kernel_value::tokens::TokenReader;
use lash_kernel_value::tokens::TokenWriter;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub use lash_kernel_value::sites::WaitId;
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
        use_: OperationUse,
        args: Vec<Datum>,
    },
}

/// One admitted wait, keyed by its effect identity: what a parent hands a
/// worker beside a checkpoint root to resume it.
#[derive(Clone, Debug, PartialEq)]
pub struct AdmittedWait {
    pub identity: EffectId,
    pub request: AdmittedRequest,
}

impl AdmittedWait {
    pub fn encode(&self) -> Vec<u8> {
        lash_kernel_value::tokens::encode_body(self)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, TokenError> {
        lash_kernel_value::tokens::decode_body(bytes)
    }
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
            Self::Effect { use_, args } => {
                out.sum(0, 2);
                use_.encode(out);
                args.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| match variant {
                0 => Some(2),
                _ => None,
            })? {
                0 => Self::Effect {
                    use_: OperationUse::decode(input)?,
                    args: Vec::decode(input)?,
                },
                _ => unreachable!(),
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
            identity: EffectId::decode(input)?,
            request: AdmittedRequest::decode(input)?,
        })
    }
}

/// Deliberately invalid inputs for end-to-end persistence refusal laws.
pub mod testkit {
    /// Changes only the kernel version in a checkpoint's token body. Used by
    /// the retained-image stale-state refusal law; no execution records escape.
    pub fn checkpoint_with_kernel(
        bytes: &[u8],
        kernel: u32,
    ) -> Result<Vec<u8>, lash_kernel_value::tokens::TokenError> {
        let mut root: crate::paged::CheckpointRoot = lash_kernel_value::tokens::decode_body(bytes)?;
        root.kernel = kernel;
        Ok(lash_kernel_value::tokens::encode_body(&root))
    }
}
