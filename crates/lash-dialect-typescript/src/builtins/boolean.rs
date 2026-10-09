use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/boolean.kernel"),
        rows: vec![
            Row::Function {
                path: "Boolean",
                function: "ts.boolean.convert",
            },
            Row::Method {
                receiver: Receiver::Bool,
                name: "valueOf",
                function: "ts.boolean.valueOf",
            },
            Row::Function {
                path: "Boolean.prototype.valueOf",
                function: "ts.boolean.valueOf",
            },
            Row::Method {
                receiver: Receiver::Bool,
                name: "toString",
                function: "ts.boolean.toString",
            },
            Row::Function {
                path: "Boolean.prototype.toString",
                function: "ts.boolean.toString",
            },
        ],
    }
}
