use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/regexp.kernel"),
        rows: vec![
            Row::Function {
                path: "String.prototype.match",
                function: "ts.string.match",
            },
            Row::Function {
                path: "String.prototype.matchAll",
                function: "ts.string.matchAll",
            },
            Row::Function {
                path: "String.prototype.search",
                function: "ts.string.search",
            },
            Row::Function {
                path: "String.prototype.split",
                function: "ts.string.split",
            },
            Row::Function {
                path: "String.prototype.replace",
                function: "ts.string.replace",
            },
            Row::Function {
                path: "String.prototype.replaceAll",
                function: "ts.string.replaceAll",
            },
            Row::Function {
                path: "RegExp.prototype.exec",
                function: "ts.regexp.exec",
            },
            Row::Function {
                path: "RegExp.prototype.test",
                function: "ts.regexp.test",
            },
            Row::Function {
                path: "RegExp.prototype.toString",
                function: "ts.regexp.toString",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "match",
                function: "ts.string.match",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "matchAll",
                function: "ts.string.matchAll",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "search",
                function: "ts.string.search",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "split",
                function: "ts.string.split",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "replace",
                function: "ts.string.replace",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "replaceAll",
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
                receiver: Receiver::Brand("regex.ecma"),
                name: "exec",
                function: "ts.regexp.exec",
            },
            Row::Method {
                receiver: Receiver::Brand("regex.ecma"),
                name: "test",
                function: "ts.regexp.test",
            },
            Row::Method {
                receiver: Receiver::Brand("regex.ecma"),
                name: "toString",
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
