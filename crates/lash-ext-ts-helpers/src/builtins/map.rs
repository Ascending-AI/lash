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
                path: "Map.prototype.set",
                receivers: &[Receiver::Map],
                function: "ts.map.set",
            },
            Row::Method {
                path: "Map.prototype.get",
                receivers: &[Receiver::Map],
                function: "ts.map.get",
            },
            Row::Method {
                path: "Map.prototype.has",
                receivers: &[Receiver::Map],
                function: "ts.map.has",
            },
            Row::Method {
                path: "Map.prototype.delete",
                receivers: &[Receiver::Map],
                function: "ts.map.delete",
            },
            Row::Method {
                path: "Map.prototype.clear",
                receivers: &[Receiver::Map],
                function: "ts.map.clear",
            },
            Row::Method {
                path: "Map.prototype.keys",
                receivers: &[Receiver::Map],
                function: "ts.map.keys",
            },
            Row::Method {
                path: "Map.prototype.values",
                receivers: &[Receiver::Map],
                function: "ts.map.values",
            },
            Row::Method {
                path: "Map.prototype.entries",
                receivers: &[Receiver::Map],
                function: "ts.map.entries",
            },
            Row::Method {
                path: "Map.prototype.forEach",
                receivers: &[Receiver::Map],
                function: "ts.map.forEach",
            },
            Row::Function {
                path: "Map.groupBy",
                function: "ts.map.groupBy",
            },
        ],
    }
}
