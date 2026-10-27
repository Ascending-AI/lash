use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/bigint.kernel"),
        rows: vec![
            Row::Function {
                path: "BigInt",
                length: 1,
                function: "ts.bigint.convert",
            },
            Row::Function {
                path: "BigInt.asIntN",
                length: 2,
                function: "ts.bigint.asIntN",
            },
            Row::Function {
                path: "BigInt.asUintN",
                length: 2,
                function: "ts.bigint.asUintN",
            },
            Row::Method {
                path: "BigInt.prototype.valueOf",
                length: 0,
                receivers: &[Receiver::BigInt],
                function: "ts.bigint.valueOf",
            },
            Row::Method {
                path: "BigInt.prototype.toString",
                length: 0,
                receivers: &[Receiver::BigInt],
                function: "ts.bigint.toString",
            },
        ],
    }
}
