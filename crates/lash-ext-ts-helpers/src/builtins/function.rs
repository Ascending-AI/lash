use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/function.kernel"),
        rows: vec![
            Row::Function {
                path: "eval",
                function: "ts.function.eval",
            },
            Row::Method {
                path: "Function.prototype.call",
                receivers: &[Receiver::Closure],
                function: "ts.function.call",
            },
            Row::Method {
                path: "Function.prototype.apply",
                receivers: &[Receiver::Closure],
                function: "ts.function.apply",
            },
            Row::Method {
                path: "Function.prototype.bind",
                receivers: &[Receiver::Closure],
                function: "ts.function.bind",
            },
            Row::Method {
                path: "Function.prototype.toString",
                receivers: &[Receiver::Closure],
                function: "ts.function.toString",
            },
        ],
    }
}
