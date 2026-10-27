use super::{Object, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/math.kernel"),
        rows: vec![
            Row::Function {
                path: "Math.abs",
                length: 1,
                function: "ts.math.abs",
            },
            Row::Function {
                path: "Math.floor",
                length: 1,
                function: "ts.math.floor",
            },
            Row::Function {
                path: "Math.ceil",
                length: 1,
                function: "ts.math.ceil",
            },
            Row::Function {
                path: "Math.trunc",
                length: 1,
                function: "ts.math.trunc",
            },
            Row::Function {
                path: "Math.sign",
                length: 1,
                function: "ts.math.sign",
            },
            Row::Function {
                path: "Math.acos",
                length: 1,
                function: "ts.math.acos",
            },
            Row::Function {
                path: "Math.asin",
                length: 1,
                function: "ts.math.asin",
            },
            Row::Function {
                path: "Math.acosh",
                length: 1,
                function: "ts.math.acosh",
            },
            Row::Function {
                path: "Math.asinh",
                length: 1,
                function: "ts.math.asinh",
            },
            Row::Function {
                path: "Math.atan",
                length: 1,
                function: "ts.math.atan",
            },
            Row::Function {
                path: "Math.atanh",
                length: 1,
                function: "ts.math.atanh",
            },
            Row::Function {
                path: "Math.cbrt",
                length: 1,
                function: "ts.math.cbrt",
            },
            Row::Function {
                path: "Math.cos",
                length: 1,
                function: "ts.math.cos",
            },
            Row::Function {
                path: "Math.cosh",
                length: 1,
                function: "ts.math.cosh",
            },
            Row::Function {
                path: "Math.exp",
                length: 1,
                function: "ts.math.exp",
            },
            Row::Function {
                path: "Math.expm1",
                length: 1,
                function: "ts.math.expm1",
            },
            Row::Function {
                path: "Math.log",
                length: 1,
                function: "ts.math.log",
            },
            Row::Function {
                path: "Math.log1p",
                length: 1,
                function: "ts.math.log1p",
            },
            Row::Function {
                path: "Math.log10",
                length: 1,
                function: "ts.math.log10",
            },
            Row::Function {
                path: "Math.log2",
                length: 1,
                function: "ts.math.log2",
            },
            Row::Function {
                path: "Math.sin",
                length: 1,
                function: "ts.math.sin",
            },
            Row::Function {
                path: "Math.sinh",
                length: 1,
                function: "ts.math.sinh",
            },
            Row::Function {
                path: "Math.tan",
                length: 1,
                function: "ts.math.tan",
            },
            Row::Function {
                path: "Math.tanh",
                length: 1,
                function: "ts.math.tanh",
            },
            Row::Function {
                path: "Math.sqrt",
                length: 1,
                function: "ts.math.sqrt",
            },
            Row::Function {
                path: "Math.round",
                length: 1,
                function: "ts.math.round",
            },
            Row::Function {
                path: "Math.atan2",
                length: 2,
                function: "ts.math.atan2",
            },
            Row::Function {
                path: "Math.pow",
                length: 2,
                function: "ts.math.pow",
            },
            Row::Function {
                path: "Math.max",
                length: 2,
                function: "ts.math.max",
            },
            Row::Function {
                path: "Math.min",
                length: 2,
                function: "ts.math.min",
            },
            Row::Function {
                path: "Math.hypot",
                length: 2,
                function: "ts.math.hypot",
            },
            Row::Function {
                path: "Math.imul",
                length: 2,
                function: "ts.math.imul",
            },
            Row::Function {
                path: "Math.clz32",
                length: 1,
                function: "ts.math.clz32",
            },
            Row::Function {
                path: "Math.fround",
                length: 1,
                function: "ts.math.fround",
            },
            Row::Function {
                path: "Math.random",
                length: 0,
                function: "ts.math.random",
            },
            Row::Value {
                path: "Math.E",
                function: "ts.math.E",
            },
            Row::Value {
                path: "Math.LN10",
                function: "ts.math.LN10",
            },
            Row::Value {
                path: "Math.LN2",
                function: "ts.math.LN2",
            },
            Row::Value {
                path: "Math.LOG10E",
                function: "ts.math.LOG10E",
            },
            Row::Value {
                path: "Math.LOG2E",
                function: "ts.math.LOG2E",
            },
            Row::Value {
                path: "Math.PI",
                function: "ts.math.PI",
            },
            Row::Value {
                path: "Math.SQRT1_2",
                function: "ts.math.SQRT1_2",
            },
            Row::Value {
                path: "Math.SQRT2",
                function: "ts.math.SQRT2",
            },
        ],
    }
}
