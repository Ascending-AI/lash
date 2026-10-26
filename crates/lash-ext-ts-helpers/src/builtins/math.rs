use super::{Object, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/math.kernel"),
        rows: vec![
            Row::Function {
                path: "Math.abs",
                function: "ts.math.abs",
            },
            Row::Function {
                path: "Math.floor",
                function: "ts.math.floor",
            },
            Row::Function {
                path: "Math.ceil",
                function: "ts.math.ceil",
            },
            Row::Function {
                path: "Math.trunc",
                function: "ts.math.trunc",
            },
            Row::Function {
                path: "Math.sign",
                function: "ts.math.sign",
            },
            Row::Function {
                path: "Math.acos",
                function: "ts.math.acos",
            },
            Row::Function {
                path: "Math.asin",
                function: "ts.math.asin",
            },
            Row::Function {
                path: "Math.acosh",
                function: "ts.math.acosh",
            },
            Row::Function {
                path: "Math.asinh",
                function: "ts.math.asinh",
            },
            Row::Function {
                path: "Math.atan",
                function: "ts.math.atan",
            },
            Row::Function {
                path: "Math.atanh",
                function: "ts.math.atanh",
            },
            Row::Function {
                path: "Math.cbrt",
                function: "ts.math.cbrt",
            },
            Row::Function {
                path: "Math.cos",
                function: "ts.math.cos",
            },
            Row::Function {
                path: "Math.cosh",
                function: "ts.math.cosh",
            },
            Row::Function {
                path: "Math.exp",
                function: "ts.math.exp",
            },
            Row::Function {
                path: "Math.expm1",
                function: "ts.math.expm1",
            },
            Row::Function {
                path: "Math.log",
                function: "ts.math.log",
            },
            Row::Function {
                path: "Math.log1p",
                function: "ts.math.log1p",
            },
            Row::Function {
                path: "Math.log10",
                function: "ts.math.log10",
            },
            Row::Function {
                path: "Math.log2",
                function: "ts.math.log2",
            },
            Row::Function {
                path: "Math.sin",
                function: "ts.math.sin",
            },
            Row::Function {
                path: "Math.sinh",
                function: "ts.math.sinh",
            },
            Row::Function {
                path: "Math.tan",
                function: "ts.math.tan",
            },
            Row::Function {
                path: "Math.tanh",
                function: "ts.math.tanh",
            },
            Row::Function {
                path: "Math.sqrt",
                function: "ts.math.sqrt",
            },
            Row::Function {
                path: "Math.round",
                function: "ts.math.round",
            },
            Row::Function {
                path: "Math.atan2",
                function: "ts.math.atan2",
            },
            Row::Function {
                path: "Math.pow",
                function: "ts.math.pow",
            },
            Row::Function {
                path: "Math.max",
                function: "ts.math.max",
            },
            Row::Function {
                path: "Math.min",
                function: "ts.math.min",
            },
            Row::Function {
                path: "Math.hypot",
                function: "ts.math.hypot",
            },
            Row::Function {
                path: "Math.imul",
                function: "ts.math.imul",
            },
            Row::Function {
                path: "Math.clz32",
                function: "ts.math.clz32",
            },
            Row::Function {
                path: "Math.fround",
                function: "ts.math.fround",
            },
            Row::Function {
                path: "Math.random",
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
