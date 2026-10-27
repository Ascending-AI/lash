use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/set.kernel"),
        rows: vec![
            Row::Constructor {
                class: "Set",
                length: None,
                function: "ts.set.construct",
            },
            Row::Function {
                path: "Set",
                length: 0,
                function: "ts.set.call",
            },
            Row::InstanceOf {
                class: "Set",
                function: "ts.set.is",
            },
            Row::Property {
                receiver: Receiver::Set,
                name: "size",
                function: "ts.set.size",
            },
            Row::Method {
                path: "Set.prototype.add",
                length: 1,
                receivers: &[Receiver::Set],
                function: "ts.set.add",
            },
            Row::Method {
                path: "Set.prototype.has",
                length: 1,
                receivers: &[Receiver::Set],
                function: "ts.set.has",
            },
            Row::Method {
                path: "Set.prototype.delete",
                length: 1,
                receivers: &[Receiver::Set],
                function: "ts.set.delete",
            },
            Row::Method {
                path: "Set.prototype.clear",
                length: 0,
                receivers: &[Receiver::Set],
                function: "ts.set.clear",
            },
            Row::Method {
                path: "Set.prototype.keys",
                length: 0,
                receivers: &[Receiver::Set],
                function: "ts.set.values",
            },
            Row::Method {
                path: "Set.prototype.values",
                length: 0,
                receivers: &[Receiver::Set],
                function: "ts.set.values",
            },
            Row::Method {
                path: "Set.prototype.entries",
                length: 0,
                receivers: &[Receiver::Set],
                function: "ts.set.entries",
            },
            Row::Method {
                path: "Set.prototype.forEach",
                length: 1,
                receivers: &[Receiver::Set],
                function: "ts.set.forEach",
            },
            Row::Method {
                path: "Set.prototype.union",
                length: 1,
                receivers: &[Receiver::Set],
                function: "ts.set.union",
            },
            Row::Method {
                path: "Set.prototype.intersection",
                length: 1,
                receivers: &[Receiver::Set],
                function: "ts.set.intersection",
            },
            Row::Method {
                path: "Set.prototype.difference",
                length: 1,
                receivers: &[Receiver::Set],
                function: "ts.set.difference",
            },
            Row::Method {
                path: "Set.prototype.symmetricDifference",
                length: 1,
                receivers: &[Receiver::Set],
                function: "ts.set.symmetricDifference",
            },
            Row::Method {
                path: "Set.prototype.isSubsetOf",
                length: 1,
                receivers: &[Receiver::Set],
                function: "ts.set.isSubsetOf",
            },
            Row::Method {
                path: "Set.prototype.isSupersetOf",
                length: 1,
                receivers: &[Receiver::Set],
                function: "ts.set.isSupersetOf",
            },
            Row::Method {
                path: "Set.prototype.isDisjointFrom",
                length: 1,
                receivers: &[Receiver::Set],
                function: "ts.set.isDisjointFrom",
            },
        ],
    }
}
