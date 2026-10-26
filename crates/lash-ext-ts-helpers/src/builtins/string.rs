use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/string.kernel"),
        rows: vec![
            Row::Method {
                path: "String.prototype.localeCompare",
                receivers: &[Receiver::Text],
                function: "ts.string.locale_unsupported",
            },
            Row::Method {
                path: "String.prototype.toLocaleLowerCase",
                receivers: &[Receiver::Text],
                function: "ts.string.toLocaleLowerCase",
            },
            Row::Method {
                path: "String.prototype.toLocaleUpperCase",
                receivers: &[Receiver::Text],
                function: "ts.string.toLocaleUpperCase",
            },
            Row::Method {
                path: "String.prototype.normalize",
                receivers: &[Receiver::Text],
                function: "ts.string.normalize",
            },
            Row::Method {
                path: "String.prototype.at",
                receivers: &[Receiver::Text],
                function: "ts.string.at",
            },
            Row::Method {
                path: "String.prototype.charAt",
                receivers: &[Receiver::Text],
                function: "ts.string.charAt",
            },
            Row::Method {
                path: "String.prototype.charCodeAt",
                receivers: &[Receiver::Text],
                function: "ts.string.charCodeAt",
            },
            Row::Method {
                path: "String.prototype.codePointAt",
                receivers: &[Receiver::Text],
                function: "ts.string.codePointAt",
            },
            Row::Method {
                path: "String.prototype.slice",
                receivers: &[Receiver::Text],
                function: "ts.string.slice",
            },
            Row::Method {
                path: "String.prototype.substring",
                receivers: &[Receiver::Text],
                function: "ts.string.substring",
            },
            Row::Method {
                path: "String.prototype.indexOf",
                receivers: &[Receiver::Text],
                function: "ts.string.indexOf",
            },
            Row::Method {
                path: "String.prototype.lastIndexOf",
                receivers: &[Receiver::Text],
                function: "ts.string.lastIndexOf",
            },
            Row::Method {
                path: "String.prototype.includes",
                receivers: &[Receiver::Text],
                function: "ts.string.includes",
            },
            Row::Method {
                path: "String.prototype.startsWith",
                receivers: &[Receiver::Text],
                function: "ts.string.startsWith",
            },
            Row::Method {
                path: "String.prototype.endsWith",
                receivers: &[Receiver::Text],
                function: "ts.string.endsWith",
            },
            Row::Method {
                path: "String.prototype.concat",
                receivers: &[Receiver::Text],
                function: "ts.string.concat",
            },
            Row::Method {
                path: "String.prototype.toLowerCase",
                receivers: &[Receiver::Text],
                function: "ts.string.toLowerCase",
            },
            Row::Method {
                path: "String.prototype.toUpperCase",
                receivers: &[Receiver::Text],
                function: "ts.string.toUpperCase",
            },
            Row::Method {
                path: "String.prototype.trim",
                receivers: &[Receiver::Text],
                function: "ts.string.trim",
            },
            Row::Method {
                path: "String.prototype.trimStart",
                receivers: &[Receiver::Text],
                function: "ts.string.trimStart",
            },
            Row::Method {
                path: "String.prototype.trimEnd",
                receivers: &[Receiver::Text],
                function: "ts.string.trimEnd",
            },
            Row::Method {
                path: "String.prototype.repeat",
                receivers: &[Receiver::Text],
                function: "ts.string.repeat",
            },
            Row::Method {
                path: "String.prototype.padStart",
                receivers: &[Receiver::Text],
                function: "ts.string.padStart",
            },
            Row::Method {
                path: "String.prototype.padEnd",
                receivers: &[Receiver::Text],
                function: "ts.string.padEnd",
            },
            Row::Method {
                path: "String.prototype.toString",
                receivers: &[Receiver::Text],
                function: "ts.string.toString",
            },
            Row::Method {
                path: "String.prototype.valueOf",
                receivers: &[Receiver::Text],
                function: "ts.string.valueOf",
            },
            Row::Property {
                receiver: Receiver::Text,
                name: "length",
                function: "ts.string.length",
            },
            Row::Function {
                path: "String",
                function: "ts.string.create",
            },
            Row::Function {
                path: "String.fromCharCode",
                function: "ts.string.fromCharCode",
            },
            Row::Function {
                path: "String.fromCodePoint",
                function: "ts.string.fromCodePoint",
            },
            Row::Function {
                path: "String.raw",
                function: "ts.string.raw",
            },
        ],
    }
}
