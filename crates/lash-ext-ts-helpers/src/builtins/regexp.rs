use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/regexp.kernel"),
        rows: vec![
            Row::Method {
                path: "String.prototype.match",
                length: 1,
                receivers: &[Receiver::Text],
                function: "ts.string.match",
            },
            Row::Method {
                path: "String.prototype.matchAll",
                length: 1,
                receivers: &[Receiver::Text],
                function: "ts.string.matchAll",
            },
            Row::Method {
                path: "String.prototype.search",
                length: 1,
                receivers: &[Receiver::Text],
                function: "ts.string.search",
            },
            Row::Method {
                path: "String.prototype.split",
                length: 2,
                receivers: &[Receiver::Text],
                function: "ts.string.split",
            },
            Row::Method {
                path: "String.prototype.replace",
                length: 2,
                receivers: &[Receiver::Text],
                function: "ts.string.replace",
            },
            Row::Method {
                path: "String.prototype.replaceAll",
                length: 2,
                receivers: &[Receiver::Text],
                function: "ts.string.replaceAll",
            },
            Row::Function {
                path: "RegExp",
                length: 2,
                function: "ts.regexp.call",
            },
            Row::Constructor {
                class: "RegExp",
                length: None,
                function: "ts.regexp.create",
            },
            Row::InstanceOf {
                class: "RegExp",
                function: "ts.regexp.is",
            },
            Row::Method {
                path: "RegExp.prototype.exec",
                length: 1,
                receivers: &[Receiver::Brand("regex.ecma.reg_exp")],
                function: "ts.regexp.exec",
            },
            Row::Method {
                path: "RegExp.prototype.test",
                length: 1,
                receivers: &[Receiver::Brand("regex.ecma.reg_exp")],
                function: "ts.regexp.test",
            },
            Row::Method {
                path: "RegExp.prototype.toString",
                length: 0,
                receivers: &[Receiver::Brand("regex.ecma.reg_exp")],
                function: "ts.regexp.toString",
            },
            Row::Property {
                receiver: Receiver::Brand("regex.ecma.reg_exp"),
                name: "hasIndices",
                function: "ts.regexp.hasIndices",
            },
            Row::Property {
                receiver: Receiver::Brand("regex.ecma.reg_exp"),
                name: "source",
                function: "ts.regexp.source",
            },
            Row::Property {
                receiver: Receiver::Brand("regex.ecma.reg_exp"),
                name: "flags",
                function: "ts.regexp.flags",
            },
            Row::Property {
                receiver: Receiver::Brand("regex.ecma.reg_exp"),
                name: "global",
                function: "ts.regexp.global",
            },
            Row::Property {
                receiver: Receiver::Brand("regex.ecma.reg_exp"),
                name: "ignoreCase",
                function: "ts.regexp.ignoreCase",
            },
            Row::Property {
                receiver: Receiver::Brand("regex.ecma.reg_exp"),
                name: "multiline",
                function: "ts.regexp.multiline",
            },
            Row::Property {
                receiver: Receiver::Brand("regex.ecma.reg_exp"),
                name: "dotAll",
                function: "ts.regexp.dotAll",
            },
            Row::Property {
                receiver: Receiver::Brand("regex.ecma.reg_exp"),
                name: "unicode",
                function: "ts.regexp.unicode",
            },
            Row::Property {
                receiver: Receiver::Brand("regex.ecma.reg_exp"),
                name: "sticky",
                function: "ts.regexp.sticky",
            },
        ],
    }
}
