use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/number.kernel"),
        rows: vec![
            Row::Method {
                path: "Number.prototype.toLocaleString",
                length: 0,
                receivers: &[Receiver::Number],
                function: "ts.number.toLocaleString",
            },
            Row::Function {
                path: "Number",
                length: 1,
                function: "ts.number.convert",
            },
            Row::Function {
                path: "Number.isFinite",
                length: 1,
                function: "ts.number.isFinite",
            },
            Row::Function {
                path: "Number.isNaN",
                length: 1,
                function: "ts.number.isNaN",
            },
            Row::Function {
                path: "Number.isInteger",
                length: 1,
                function: "ts.number.isInteger",
            },
            Row::Function {
                path: "Number.isSafeInteger",
                length: 1,
                function: "ts.number.isSafeInteger",
            },
            Row::Function {
                path: "Number.parseFloat",
                length: 1,
                function: "ts.number.parseFloat",
            },
            Row::Function {
                path: "parseFloat",
                length: 1,
                function: "ts.number.parseFloat",
            },
            Row::Function {
                path: "Number.parseInt",
                length: 2,
                function: "ts.number.parseInt",
            },
            Row::Function {
                path: "parseInt",
                length: 2,
                function: "ts.number.parseInt",
            },
            Row::Method {
                path: "Number.prototype.valueOf",
                length: 0,
                receivers: &[Receiver::Number],
                function: "ts.number.valueOf",
            },
            Row::Method {
                path: "Number.prototype.toString",
                length: 1,
                receivers: &[Receiver::Number],
                function: "ts.number.toString",
            },
            Row::Method {
                path: "Number.prototype.toFixed",
                length: 1,
                receivers: &[Receiver::Number],
                function: "ts.number.toFixed",
            },
            Row::Method {
                path: "Number.prototype.toExponential",
                length: 1,
                receivers: &[Receiver::Number],
                function: "ts.number.toExponential",
            },
            Row::Method {
                path: "Number.prototype.toPrecision",
                length: 1,
                receivers: &[Receiver::Number],
                function: "ts.number.toPrecision",
            },
            Row::Value {
                path: "Number.EPSILON",
                function: "ts.number.EPSILON",
            },
            Row::Value {
                path: "Number.MAX_SAFE_INTEGER",
                function: "ts.number.MAX_SAFE_INTEGER",
            },
            Row::Value {
                path: "Number.MIN_SAFE_INTEGER",
                function: "ts.number.MIN_SAFE_INTEGER",
            },
            Row::Value {
                path: "Number.MAX_VALUE",
                function: "ts.number.MAX_VALUE",
            },
            Row::Value {
                path: "Number.MIN_VALUE",
                function: "ts.number.MIN_VALUE",
            },
            Row::Value {
                path: "Number.NaN",
                function: "ts.number.NaN",
            },
            Row::Value {
                path: "Number.POSITIVE_INFINITY",
                function: "ts.number.POSITIVE_INFINITY",
            },
            Row::Value {
                path: "Number.NEGATIVE_INFINITY",
                function: "ts.number.NEGATIVE_INFINITY",
            },
            Row::Function {
                path: "isFinite",
                length: 1,
                function: "ts.number.global_isFinite",
            },
            Row::Function {
                path: "isNaN",
                length: 1,
                function: "ts.number.global_isNaN",
            },
        ],
    }
}
