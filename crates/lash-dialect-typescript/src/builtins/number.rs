use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/number.kernel"),
        rows: vec![
            Row::Function {
                path: "Number",
                function: "ts.number.convert",
            },
            Row::Function {
                path: "Number.isFinite",
                function: "ts.number.isFinite",
            },
            Row::Function {
                path: "Number.isNaN",
                function: "ts.number.isNaN",
            },
            Row::Function {
                path: "Number.isInteger",
                function: "ts.number.isInteger",
            },
            Row::Function {
                path: "Number.isSafeInteger",
                function: "ts.number.isSafeInteger",
            },
            Row::Function {
                path: "Number.parseFloat",
                function: "ts.number.parseFloat",
            },
            Row::Function {
                path: "parseFloat",
                function: "ts.number.parseFloat",
            },
            Row::Function {
                path: "Number.parseInt",
                function: "ts.number.parseInt",
            },
            Row::Function {
                path: "parseInt",
                function: "ts.number.parseInt",
            },
            Row::Method {
                receiver: Receiver::Number,
                name: "valueOf",
                function: "ts.number.valueOf",
            },
            Row::Function {
                path: "Number.prototype.valueOf",
                function: "ts.number.valueOf",
            },
            Row::Method {
                receiver: Receiver::Number,
                name: "toString",
                function: "ts.number.toString",
            },
            Row::Function {
                path: "Number.prototype.toString",
                function: "ts.number.toString",
            },
            Row::Method {
                receiver: Receiver::Number,
                name: "toFixed",
                function: "ts.number.toFixed",
            },
            Row::Function {
                path: "Number.prototype.toFixed",
                function: "ts.number.toFixed",
            },
            Row::Method {
                receiver: Receiver::Number,
                name: "toExponential",
                function: "ts.number.toExponential",
            },
            Row::Function {
                path: "Number.prototype.toExponential",
                function: "ts.number.toExponential",
            },
            Row::Method {
                receiver: Receiver::Number,
                name: "toPrecision",
                function: "ts.number.toPrecision",
            },
            Row::Function {
                path: "Number.prototype.toPrecision",
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
                function: "ts.number.global_isFinite",
            },
            Row::Function {
                path: "isNaN",
                function: "ts.number.global_isNaN",
            },
        ],
    }
}
