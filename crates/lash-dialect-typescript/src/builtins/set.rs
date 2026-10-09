use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/set.kernel"),
        rows: vec![
            Row::Constructor {
                class: "Set",
                function: "ts.set.construct",
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
                receiver: Receiver::Set,
                name: "add",
                function: "ts.set.add",
            },
            Row::Function {
                path: "Set.prototype.add",
                function: "ts.set.add",
            },
            Row::Method {
                receiver: Receiver::Set,
                name: "has",
                function: "ts.set.has",
            },
            Row::Function {
                path: "Set.prototype.has",
                function: "ts.set.has",
            },
            Row::Method {
                receiver: Receiver::Set,
                name: "delete",
                function: "ts.set.delete",
            },
            Row::Function {
                path: "Set.prototype.delete",
                function: "ts.set.delete",
            },
            Row::Method {
                receiver: Receiver::Set,
                name: "clear",
                function: "ts.set.clear",
            },
            Row::Function {
                path: "Set.prototype.clear",
                function: "ts.set.clear",
            },
            Row::Method {
                receiver: Receiver::Set,
                name: "keys",
                function: "ts.set.keys",
            },
            Row::Function {
                path: "Set.prototype.keys",
                function: "ts.set.keys",
            },
            Row::Method {
                receiver: Receiver::Set,
                name: "values",
                function: "ts.set.values",
            },
            Row::Function {
                path: "Set.prototype.values",
                function: "ts.set.values",
            },
            Row::Method {
                receiver: Receiver::Set,
                name: "entries",
                function: "ts.set.entries",
            },
            Row::Function {
                path: "Set.prototype.entries",
                function: "ts.set.entries",
            },
            Row::Method {
                receiver: Receiver::Set,
                name: "forEach",
                function: "ts.set.forEach",
            },
            Row::Function {
                path: "Set.prototype.forEach",
                function: "ts.set.forEach",
            },
            Row::Method {
                receiver: Receiver::Set,
                name: "union",
                function: "ts.set.union",
            },
            Row::Function {
                path: "Set.prototype.union",
                function: "ts.set.union",
            },
            Row::Method {
                receiver: Receiver::Set,
                name: "intersection",
                function: "ts.set.intersection",
            },
            Row::Function {
                path: "Set.prototype.intersection",
                function: "ts.set.intersection",
            },
            Row::Method {
                receiver: Receiver::Set,
                name: "difference",
                function: "ts.set.difference",
            },
            Row::Function {
                path: "Set.prototype.difference",
                function: "ts.set.difference",
            },
            Row::Method {
                receiver: Receiver::Set,
                name: "symmetricDifference",
                function: "ts.set.symmetricDifference",
            },
            Row::Function {
                path: "Set.prototype.symmetricDifference",
                function: "ts.set.symmetricDifference",
            },
            Row::Method {
                receiver: Receiver::Set,
                name: "isSubsetOf",
                function: "ts.set.isSubsetOf",
            },
            Row::Function {
                path: "Set.prototype.isSubsetOf",
                function: "ts.set.isSubsetOf",
            },
            Row::Method {
                receiver: Receiver::Set,
                name: "isSupersetOf",
                function: "ts.set.isSupersetOf",
            },
            Row::Function {
                path: "Set.prototype.isSupersetOf",
                function: "ts.set.isSupersetOf",
            },
            Row::Method {
                receiver: Receiver::Set,
                name: "isDisjointFrom",
                function: "ts.set.isDisjointFrom",
            },
            Row::Function {
                path: "Set.prototype.isDisjointFrom",
                function: "ts.set.isDisjointFrom",
            },
        ],
    }
}
