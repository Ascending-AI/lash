use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/regexp.kernel"),
        rows: vec![
            Row::Method {
                path: "String.prototype.match",
                receivers: &[Receiver::Text],
                function: "ts.string.match",
            },
            Row::Method {
                path: "String.prototype.matchAll",
                receivers: &[Receiver::Text],
                function: "ts.string.matchAll",
            },
            Row::Method {
                path: "String.prototype.search",
                receivers: &[Receiver::Text],
                function: "ts.string.search",
            },
            Row::Method {
                path: "String.prototype.split",
                receivers: &[Receiver::Text],
                function: "ts.string.split",
            },
            Row::Method {
                path: "String.prototype.replace",
                receivers: &[Receiver::Text],
                function: "ts.string.replace",
            },
            Row::Method {
                path: "String.prototype.replaceAll",
                receivers: &[Receiver::Text],
                function: "ts.string.replaceAll",
            },
            Row::Function {
                path: "RegExp",
                function: "ts.regexp.call",
            },
            Row::Constructor {
                class: "RegExp",
                function: "ts.regexp.create",
            },
            Row::InstanceOf {
                class: "RegExp",
                function: "ts.regexp.is",
            },
            Row::Method {
                path: "RegExp.prototype.exec",
                receivers: &[Receiver::Brand("regex.ecma")],
                function: "ts.regexp.exec",
            },
            Row::Method {
                path: "RegExp.prototype.test",
                receivers: &[Receiver::Brand("regex.ecma")],
                function: "ts.regexp.test",
            },
            Row::Method {
                path: "RegExp.prototype.toString",
                receivers: &[Receiver::Brand("regex.ecma")],
                function: "ts.regexp.toString",
            },
            Row::Property {
                receiver: Receiver::Brand("regex.ecma"),
                name: "source",
                function: "ts.regexp.source",
            },
            Row::Property {
                receiver: Receiver::Brand("regex.ecma"),
                name: "flags",
                function: "ts.regexp.flags",
            },
            Row::Property {
                receiver: Receiver::Brand("regex.ecma"),
                name: "global",
                function: "ts.regexp.global",
            },
            Row::Property {
                receiver: Receiver::Brand("regex.ecma"),
                name: "ignoreCase",
                function: "ts.regexp.ignoreCase",
            },
            Row::Property {
                receiver: Receiver::Brand("regex.ecma"),
                name: "multiline",
                function: "ts.regexp.multiline",
            },
            Row::Property {
                receiver: Receiver::Brand("regex.ecma"),
                name: "dotAll",
                function: "ts.regexp.dotAll",
            },
            Row::Property {
                receiver: Receiver::Brand("regex.ecma"),
                name: "unicode",
                function: "ts.regexp.unicode",
            },
            Row::Property {
                receiver: Receiver::Brand("regex.ecma"),
                name: "sticky",
                function: "ts.regexp.sticky",
            },
        ],
    }
}
