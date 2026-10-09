use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/url.kernel"),
        rows: vec![
            Row::Constructor {
                class: "URL",
                function: "ts.url.construct",
            },
            Row::InstanceOf {
                class: "URL",
                function: "ts.url.is",
            },
            Row::Function {
                path: "URL.canParse",
                function: "ts.url.canParse",
            },
            Row::Method {
                receiver: Receiver::Brand("url.whatwg"),
                name: "toString",
                function: "ts.url.toString",
            },
            Row::Function {
                path: "URL.prototype.toString",
                function: "ts.url.toString",
            },
            Row::Method {
                receiver: Receiver::Brand("url.whatwg"),
                name: "toJSON",
                function: "ts.url.toJSON",
            },
            Row::Function {
                path: "URL.prototype.toJSON",
                function: "ts.url.toJSON",
            },
            Row::Method {
                receiver: Receiver::Brand("url.whatwg"),
                name: "valueOf",
                function: "ts.url.valueOf",
            },
            Row::Function {
                path: "URL.prototype.valueOf",
                function: "ts.url.valueOf",
            },
        ],
    }
}
