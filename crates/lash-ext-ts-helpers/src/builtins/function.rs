use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/function.kernel"),
        rows: vec![
            Row::Class {
                class: "Function",
                length: 1,
            },
            Row::Function {
                path: "eval",
                length: 1,
                function: "ts.function.eval",
            },
            Row::Method {
                path: "Function.prototype.call",
                length: 1,
                receivers: &[Receiver::Closure],
                function: "ts.function.call",
            },
            Row::Method {
                path: "Function.prototype.apply",
                length: 2,
                receivers: &[Receiver::Closure],
                function: "ts.function.apply",
            },
            Row::Method {
                path: "Function.prototype.bind",
                length: 1,
                receivers: &[Receiver::Closure],
                function: "ts.function.bind",
            },
            Row::Method {
                path: "Function.prototype.toString",
                length: 0,
                receivers: &[Receiver::Closure],
                function: "ts.function.toString",
            },
        ],
    }
}
