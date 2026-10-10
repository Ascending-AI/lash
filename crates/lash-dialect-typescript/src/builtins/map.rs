use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/map.kernel"),
        rows: vec![
            Row::Constructor {
                class: "Map",
                function: "ts.map.construct",
            },
            Row::Function {
                path: "Map",
                function: "ts.map.call",
            },
            Row::InstanceOf {
                class: "Map",
                function: "ts.map.is",
            },
            Row::Property {
                receiver: Receiver::Map,
                name: "size",
                function: "ts.map.size",
            },
            Row::Method {
                receiver: Receiver::Map,
                name: "set",
                function: "ts.map.set",
            },
            Row::Function {
                path: "Map.prototype.set",
                function: "ts.map.set",
            },
            Row::Method {
                receiver: Receiver::Map,
                name: "get",
                function: "ts.map.get",
            },
            Row::Function {
                path: "Map.prototype.get",
                function: "ts.map.get",
            },
            Row::Method {
                receiver: Receiver::Map,
                name: "has",
                function: "ts.map.has",
            },
            Row::Function {
                path: "Map.prototype.has",
                function: "ts.map.has",
            },
            Row::Method {
                receiver: Receiver::Map,
                name: "delete",
                function: "ts.map.delete",
            },
            Row::Function {
                path: "Map.prototype.delete",
                function: "ts.map.delete",
            },
            Row::Method {
                receiver: Receiver::Map,
                name: "clear",
                function: "ts.map.clear",
            },
            Row::Function {
                path: "Map.prototype.clear",
                function: "ts.map.clear",
            },
            Row::Method {
                receiver: Receiver::Map,
                name: "keys",
                function: "ts.map.keys",
            },
            Row::Function {
                path: "Map.prototype.keys",
                function: "ts.map.keys",
            },
            Row::Method {
                receiver: Receiver::Map,
                name: "values",
                function: "ts.map.values",
            },
            Row::Function {
                path: "Map.prototype.values",
                function: "ts.map.values",
            },
            Row::Method {
                receiver: Receiver::Map,
                name: "entries",
                function: "ts.map.entries",
            },
            Row::Function {
                path: "Map.prototype.entries",
                function: "ts.map.entries",
            },
            Row::Method {
                receiver: Receiver::Map,
                name: "forEach",
                function: "ts.map.forEach",
            },
            Row::Function {
                path: "Map.prototype.forEach",
                function: "ts.map.forEach",
            },
            Row::Function {
                path: "Map.groupBy",
                function: "ts.map.groupBy",
            },
        ],
    }
}
