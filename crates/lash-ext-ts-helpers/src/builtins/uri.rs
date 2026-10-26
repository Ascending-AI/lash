use super::{Object, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/uri.kernel"),
        rows: vec![
            Row::Function {
                path: "encodeURI",
                function: "ts.uri.encodeURI",
            },
            Row::Function {
                path: "decodeURI",
                function: "ts.uri.decodeURI",
            },
            Row::Function {
                path: "encodeURIComponent",
                function: "ts.uri.encodeURIComponent",
            },
            Row::Function {
                path: "decodeURIComponent",
                function: "ts.uri.decodeURIComponent",
            },
        ],
    }
}
