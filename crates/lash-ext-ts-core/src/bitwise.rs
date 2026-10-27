//! Number bit operations. BigInts and coercions retain their kernel bodies.

use lash_kernel_doc::scalar::{self, WordOp};
use lash_kernel_doc::{NativeCall, NativeError, NativeFunction, PreparedNative, Value};

pub(super) struct Binary(pub(super) WordOp);

fn binary(op: WordOp, args: &[Value]) -> Option<Value> {
    let [Value::Float(a), Value::Float(b)] = args else {
        return None;
    };
    Some(scalar::words(op, *a, *b))
}

impl NativeFunction for Binary {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        binary(self.0, call.args.values()).ok_or(NativeError::Decline)
    }

    fn prepare(&self) -> Option<PreparedNative> {
        let entry: fn(&[Value]) -> Option<Value> = match self.0 {
            WordOp::And => |args| binary(WordOp::And, args),
            WordOp::Or => |args| binary(WordOp::Or, args),
            WordOp::Xor => |args| binary(WordOp::Xor, args),
            WordOp::Shl => |args| binary(WordOp::Shl, args),
            WordOp::Shr => |args| binary(WordOp::Shr, args),
            WordOp::Ushr => |args| binary(WordOp::Ushr, args),
        };
        Some(PreparedNative {
            pair: None,
            entry,
            charged_entry: None,
            operation: "Number bit operation",
        })
    }
}

pub(super) struct ToInt32;
pub(super) struct Not;

fn to_int32(args: &[Value]) -> Option<Value> {
    let [Value::Float(value)] = args else {
        return None;
    };
    Some(Value::Int(scalar::uint32(value.get()) as i32 as i64))
}

fn not(args: &[Value]) -> Option<Value> {
    let [Value::Float(value)] = args else {
        return None;
    };
    Some(Value::Float(lash_kernel_doc::Float::new(
        (!scalar::uint32(value.get())) as i32 as f64,
    )))
}

macro_rules! unary {
    ($name:ident, $entry:ident) => {
        impl NativeFunction for $name {
            fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
                $entry(call.args.values()).ok_or(NativeError::Decline)
            }
            fn prepare(&self) -> Option<PreparedNative> {
                Some(PreparedNative {
                    pair: None,
                    entry: $entry,
                    charged_entry: None,
                    operation: stringify!($entry),
                })
            }
        }
    };
}
unary!(ToInt32, to_int32);
unary!(Not, not);
