use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/boolean.kernel"),
        rows: vec![
            Row::Function {
                path: "Boolean",
                length: 1,
                function: "ts.boolean.convert",
            },
            Row::Method {
                path: "Boolean.prototype.valueOf",
                length: 0,
                receivers: &[Receiver::Bool],
                function: "ts.boolean.valueOf",
            },
            Row::Method {
                path: "Boolean.prototype.toString",
                length: 0,
                receivers: &[Receiver::Bool],
                function: "ts.boolean.toString",
            },
        ],
    }
}
