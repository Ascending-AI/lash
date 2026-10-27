use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/map.kernel"),
        rows: vec![
            Row::Constructor {
                class: "Map",
                length: None,
                function: "ts.map.construct",
            },
            Row::Function {
                path: "Map",
                length: 0,
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
                path: "Map.prototype.set",
                length: 2,
                receivers: &[Receiver::Map],
                function: "ts.map.set",
            },
            Row::Method {
                path: "Map.prototype.get",
                length: 1,
                receivers: &[Receiver::Map],
                function: "ts.map.get",
            },
            Row::Method {
                path: "Map.prototype.has",
                length: 1,
                receivers: &[Receiver::Map],
                function: "ts.map.has",
            },
            Row::Method {
                path: "Map.prototype.delete",
                length: 1,
                receivers: &[Receiver::Map],
                function: "ts.map.delete",
            },
            Row::Method {
                path: "Map.prototype.clear",
                length: 0,
                receivers: &[Receiver::Map],
                function: "ts.map.clear",
            },
            Row::Method {
                path: "Map.prototype.keys",
                length: 0,
                receivers: &[Receiver::Map],
                function: "ts.map.keys",
            },
            Row::Method {
                path: "Map.prototype.values",
                length: 0,
                receivers: &[Receiver::Map],
                function: "ts.map.values",
            },
            Row::Method {
                path: "Map.prototype.entries",
                length: 0,
                receivers: &[Receiver::Map],
                function: "ts.map.entries",
            },
            Row::Method {
                path: "Map.prototype.forEach",
                length: 1,
                receivers: &[Receiver::Map],
                function: "ts.map.forEach",
            },
            Row::Function {
                path: "Map.groupBy",
                length: 2,
                function: "ts.map.groupBy",
            },
        ],
    }
}
