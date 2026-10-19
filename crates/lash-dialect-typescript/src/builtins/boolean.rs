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
                path: "Boolean.prototype.valueOf",
                receivers: &[Receiver::Bool],
                function: "ts.boolean.valueOf",
            },
            Row::Method {
                path: "Boolean.prototype.toString",
                receivers: &[Receiver::Bool],
                function: "ts.boolean.toString",
            },
        ],
    }
}
