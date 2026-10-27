use super::{Object, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/uri.kernel"),
        rows: vec![
            Row::Function {
                path: "encodeURI",
                length: 1,
                function: "ts.uri.encodeURI",
            },
            Row::Function {
                path: "decodeURI",
                length: 1,
                function: "ts.uri.decodeURI",
            },
            Row::Function {
                path: "encodeURIComponent",
                length: 1,
                function: "ts.uri.encodeURIComponent",
            },
            Row::Function {
                path: "decodeURIComponent",
                length: 1,
                function: "ts.uri.decodeURIComponent",
            },
        ],
    }
}
