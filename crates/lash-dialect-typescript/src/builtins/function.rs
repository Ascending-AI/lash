use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/function.kernel"),
        rows: vec![
            Row::Method {
                receiver: Receiver::Closure,
                name: "call",
                function: "ts.function.call",
            },
            Row::Function {
                path: "Function.prototype.call",
                function: "ts.function.call",
            },
            Row::Method {
                receiver: Receiver::Closure,
                name: "apply",
                function: "ts.function.apply",
            },
            Row::Function {
                path: "Function.prototype.apply",
                function: "ts.function.apply",
            },
            Row::Method {
                receiver: Receiver::Closure,
                name: "bind",
                function: "ts.function.bind",
            },
            Row::Function {
                path: "Function.prototype.bind",
                function: "ts.function.bind",
            },
            Row::Method {
                receiver: Receiver::Closure,
                name: "toString",
                function: "ts.function.toString",
            },
            Row::Function {
                path: "Function.prototype.toString",
                function: "ts.function.toString",
            },
        ],
    }
}
