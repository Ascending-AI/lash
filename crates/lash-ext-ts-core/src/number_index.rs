//! Numeric indexing never coerces an object or invokes guest code.
use std::sync::Arc;

use lash_kernel_doc::{Float, NativeCall, NativeError, NativeFunction, PreparedNative, Value};

pub(super) struct NumberIndex {
    pub(super) less: Arc<dyn NativeFunction>,
}

fn floats(args: &[Value]) -> Option<Value> {
    let [Value::Float(key), Value::Float(length)] = args else {
        return None;
    };
    let key = key.get();
    Some(Value::Float(Float::new(
        if key >= 0.0 && key < length.get() && key.floor() == key {
            key + 0.0
        } else {
            -1.0
        },
    )))
}

impl NativeFunction for NumberIndex {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        if let Some(value) = floats(call.args.values()) {
            return Ok(value);
        }
        let [key, _length] = call.args.values() else {
            return Err(NativeError::Decline);
        };
        let integer = call.args.integer(key)?;
        let nonnegative = match (key, &integer) {
            (Value::Float(value), _) => value.get() >= 0.0,
            (_, Some(value)) => !value.is_negative(),
            _ => return Ok(Value::Float(Float::new(-2.0))),
        };
        if !nonnegative {
            return Ok(Value::Float(Float::new(-1.0)));
        }
        // Compare before rounding. An integer just above an exactly representable
        // length must not become an in-range key by conversion to binary64.
        let within = self.less.call(NativeCall {
            args: call.args,
            new: call.new,
            counter: call.counter,
        })?;
        if !matches!(within, Value::Bool(true)) {
            return Ok(Value::Float(Float::new(-1.0)));
        }
        if let Value::Float(value) = key
            && value.get().floor() != value.get()
        {
            return Ok(Value::Float(Float::new(-1.0)));
        }
        let value = match integer {
            Some(integer) => {
                call.new.read_whole(integer.bits().div_ceil(8))?;
                lash_kernel_lib::integer_to_float(&integer)?.get()
            }
            None => match key {
                Value::Float(value) => value.get(),
                _ => return Err(NativeError::Malformed("a numeric index changed kind")),
            },
        };
        Ok(Value::Float(Float::new(value + 0.0)))
    }

    fn prepare(&self) -> Option<PreparedNative> {
        Some(PreparedNative {
            pair: None,
            entry: floats,
            charged_entry: None,
            operation: "ts.number_index(key: Any, length: Number) -> Number",
        })
    }
}
