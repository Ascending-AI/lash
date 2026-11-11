//! Python arithmetic whose word answers are the helper body's own kernel
//! operation, past the body's operand checks.

use lash_kernel_doc::arena::native::{InlineNative, InlineOperation, ScalarDomain};
use lash_kernel_doc::scalar::{self, ScalarOp};
use lash_kernel_doc::{
    FIRST_NATIVE_VERSION, NativeCall, NativeError, NativeFunction, NativeSpecialization, Value,
};

/// `py.number.mod`, `left % right`, on two word integers: the body's
/// `rem_floor`. A zero divisor, an answer past a word and every other pair decline to
/// the body, which raises or answers them.
pub(super) struct Mod;

impl NativeFunction for Mod {
    fn specialization(&self) -> NativeSpecialization {
        NativeSpecialization {
            inline: Some(InlineNative {
                native_version: FIRST_NATIVE_VERSION,
                operation: InlineOperation::Scalar {
                    operation: ScalarOp::RemFloor,
                    domain: ScalarDomain::Words,
                },
            }),
            ..Default::default()
        }
    }

    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        match call.args.values() {
            [Value::Int(left), Value::Int(right)] => {
                scalar::ints(ScalarOp::RemFloor, *left, *right).ok_or(NativeError::Decline)
            }
            _ => Err(NativeError::Decline),
        }
    }
}
