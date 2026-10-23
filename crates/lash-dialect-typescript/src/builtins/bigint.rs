use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/bigint.kernel"),
        rows: vec![
            Row::Function {
                path: "BigInt",
                function: "ts.bigint.convert",
            },
            Row::Function {
                path: "BigInt.asIntN",
                function: "ts.bigint.asIntN",
            },
            Row::Function {
                path: "BigInt.asUintN",
                function: "ts.bigint.asUintN",
            },
            Row::Method {
                path: "BigInt.prototype.valueOf",
                receivers: &[Receiver::BigInt],
                function: "ts.bigint.valueOf",
            },
            Row::Method {
                path: "BigInt.prototype.toString",
                receivers: &[Receiver::BigInt],
                function: "ts.bigint.toString",
            },
        ],
    }
}
