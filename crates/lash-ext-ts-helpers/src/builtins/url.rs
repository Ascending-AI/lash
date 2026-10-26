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
                path: "URL.prototype.toString",
                receivers: &[Receiver::Brand("url.whatwg.url")],
                function: "ts.url.toString",
            },
            Row::Method {
                path: "URL.prototype.toJSON",
                receivers: &[Receiver::Brand("url.whatwg.url")],
                function: "ts.url.toJSON",
            },
            Row::Method {
                path: "URL.prototype.valueOf",
                receivers: &[Receiver::Brand("url.whatwg.url")],
                function: "ts.url.valueOf",
            },
        ],
    }
}
