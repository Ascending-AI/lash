use super::{Object, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/json.kernel"),
        rows: vec![
            Row::Function {
                path: "JSON.parse",
                length: 2,
                function: "ts.json.parse",
            },
            Row::Function {
                path: "JSON.stringify",
                length: 3,
                function: "ts.json.stringify",
            },
        ],
    }
}
