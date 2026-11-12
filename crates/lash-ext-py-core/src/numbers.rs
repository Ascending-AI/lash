//! Numeric helper answers declared by their bodies' scalar operations.

use lash_kernel_doc::arena::native::{InlineNative, InlineOperation, ScalarDomain};
use lash_kernel_doc::scalar::{self, ScalarOp};
use lash_kernel_doc::{
    FIRST_NATIVE_VERSION, NativeCall, NativeError, NativeFunction, NativeSpecialization, Value,
};

/// A declared operation over words, or over word/float number pairs.
/// Overflow and pairs outside the domain decline to the complete body.
pub(super) struct Pair {
    operation: ScalarOp,
    domain: ScalarDomain,
    version: u32,
}

impl Pair {
    pub(super) fn words(operation: ScalarOp) -> Self {
        Self {
            operation,
            domain: ScalarDomain::Words,
            version: FIRST_NATIVE_VERSION,
        }
    }
    pub(super) fn numbers(operation: ScalarOp, version: u32) -> Self {
        Self {
            operation,
            domain: ScalarDomain::WordsOrFloats,
            version,
        }
    }
}

impl NativeFunction for Pair {
    fn specialization(&self) -> NativeSpecialization {
        NativeSpecialization {
            inline: Some(InlineNative {
                native_version: self.version,
                operation: InlineOperation::Scalar {
                    operation: self.operation,
                    domain: self.domain,
                },
            }),
            ..Default::default()
        }
    }
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let answer = match call.args.values() {
            [Value::Int(a), Value::Int(b)] if self.domain.words() => {
                scalar::ints(self.operation, *a, *b)
            }
            [Value::Float(a), Value::Float(b)] if self.domain.floats() => {
                scalar::floats(self.operation, *a, *b)
            }
            [a, b] if self.domain.mixed() => scalar::mixed(self.operation, *a, *b),
            _ => None,
        };
        answer.ok_or(NativeError::Decline)
    }
}

/// The true-division body's float arm. Zero declines before IEEE division,
/// leaving the body's Python zero-division error intact.
pub(super) struct TrueDiv;
impl NativeFunction for TrueDiv {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        match call.args.values() {
            [Value::Float(left), Value::Float(right)] if right.get() != 0.0 => {
                scalar::floats(ScalarOp::Div, *left, *right).ok_or(NativeError::Decline)
            }
            _ => Err(NativeError::Decline),
        }
    }
}
