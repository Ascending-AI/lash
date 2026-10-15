//! FIG-6372 ruling: ECMA-262 §21.3.2 implementation-approximated Math
//! functions may differ by one ULP from Node. All special values and all
//! other function classes remain bit exact. CPython math uses the same class.
use serde_json::Value;
use std::collections::BTreeSet;

/// Function classes, not per-case tolerances. The class follows actual calls.
pub fn implementation_approximated(reached: &BTreeSet<String>) -> bool {
    const NAMES: &[&str] = &[
        "acos", "acosh", "asin", "asinh", "atan", "atan2", "atanh", "cbrt", "cos", "cosh", "exp",
        "expm1", "hypot", "log", "log10", "log1p", "log2", "pow", "sin", "sinh", "tan", "tanh",
    ];
    reached.iter().any(|name| {
        name.strip_prefix("ts.math.")
            .or_else(|| name.strip_prefix("py.math."))
            .is_some_and(|name| NAMES.contains(&name))
    })
}

pub fn matches(actual: &Value, expected: &Value, approximate: bool) -> bool {
    if actual == expected {
        return true;
    }
    if !approximate {
        return false;
    }
    match (actual, expected) {
        (Value::Object(a), Value::Object(b)) => {
            if a.get("type").and_then(Value::as_str) == Some("float")
                && b.get("type").and_then(Value::as_str) == Some("float")
            {
                let bits = |object: &serde_json::Map<String, Value>| {
                    u64::from_str_radix(object.get("bits")?.as_str()?, 16).ok()
                };
                let (Some(a), Some(b)) = (bits(a), bits(b)) else {
                    return false;
                };
                let (x, y) = (f64::from_bits(a), f64::from_bits(b));
                return x.is_finite()
                    && y.is_finite()
                    && x != 0.0
                    && y != 0.0
                    && x.is_sign_negative() == y.is_sign_negative()
                    && a.abs_diff(b) <= 1;
            }
            a.len() == b.len()
                && a.iter().all(|(key, value)| {
                    b.get(key)
                        .is_some_and(|wanted| matches(value, wanted, true))
                })
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| matches(a, b, true))
        }
        _ => false,
    }
}
