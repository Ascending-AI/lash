use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/set.kernel"),
        rows: vec![
            Row::Constructor {
                class: "Set",
                function: "ts.set.construct",
            },
            Row::Function {
                path: "Set",
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
                receivers: &[Receiver::Set],
                function: "ts.set.add",
            },
            Row::Method {
                path: "Set.prototype.has",
                receivers: &[Receiver::Set],
                function: "ts.set.has",
            },
            Row::Method {
                path: "Set.prototype.delete",
                receivers: &[Receiver::Set],
                function: "ts.set.delete",
            },
            Row::Method {
                path: "Set.prototype.clear",
                receivers: &[Receiver::Set],
                function: "ts.set.clear",
            },
            Row::Method {
                path: "Set.prototype.keys",
                receivers: &[Receiver::Set],
                function: "ts.set.values",
            },
            Row::Method {
                path: "Set.prototype.values",
                receivers: &[Receiver::Set],
                function: "ts.set.values",
            },
            Row::Method {
                path: "Set.prototype.entries",
                receivers: &[Receiver::Set],
                function: "ts.set.entries",
            },
            Row::Method {
                path: "Set.prototype.forEach",
                receivers: &[Receiver::Set],
                function: "ts.set.forEach",
            },
            Row::Method {
                path: "Set.prototype.union",
                receivers: &[Receiver::Set],
                function: "ts.set.union",
            },
            Row::Method {
                path: "Set.prototype.intersection",
                receivers: &[Receiver::Set],
                function: "ts.set.intersection",
            },
            Row::Method {
                path: "Set.prototype.difference",
                receivers: &[Receiver::Set],
                function: "ts.set.difference",
            },
            Row::Method {
                path: "Set.prototype.symmetricDifference",
                receivers: &[Receiver::Set],
                function: "ts.set.symmetricDifference",
            },
            Row::Method {
                path: "Set.prototype.isSubsetOf",
                receivers: &[Receiver::Set],
                function: "ts.set.isSubsetOf",
            },
            Row::Method {
                path: "Set.prototype.isSupersetOf",
                receivers: &[Receiver::Set],
                function: "ts.set.isSupersetOf",
            },
            Row::Method {
                path: "Set.prototype.isDisjointFrom",
                receivers: &[Receiver::Set],
                function: "ts.set.isDisjointFrom",
            },
        ],
    }
}
