use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/string.kernel"),
        rows: vec![
            Row::Method {
                path: "String.prototype.localeCompare",
                length: 1,
                receivers: &[Receiver::Text],
                function: "ts.string.locale_unsupported",
            },
            Row::Method {
                path: "String.prototype.toLocaleLowerCase",
                length: 0,
                receivers: &[Receiver::Text],
                function: "ts.string.toLocaleLowerCase",
            },
            Row::Method {
                path: "String.prototype.toLocaleUpperCase",
                length: 0,
                receivers: &[Receiver::Text],
                function: "ts.string.toLocaleUpperCase",
            },
            Row::Method {
                path: "String.prototype.normalize",
                length: 0,
                receivers: &[Receiver::Text],
                function: "ts.string.normalize",
            },
            Row::Method {
                path: "String.prototype.at",
                length: 1,
                receivers: &[Receiver::Text],
                function: "ts.string.at",
            },
            Row::Method {
                path: "String.prototype.charAt",
                length: 1,
                receivers: &[Receiver::Text],
                function: "ts.string.charAt",
            },
            Row::Method {
                path: "String.prototype.charCodeAt",
                length: 1,
                receivers: &[Receiver::Text],
                function: "ts.string.charCodeAt",
            },
            Row::Method {
                path: "String.prototype.codePointAt",
                length: 1,
                receivers: &[Receiver::Text],
                function: "ts.string.codePointAt",
            },
            Row::Method {
                path: "String.prototype.slice",
                length: 2,
                receivers: &[Receiver::Text],
                function: "ts.string.slice",
            },
            Row::Method {
                path: "String.prototype.substring",
                length: 2,
                receivers: &[Receiver::Text],
                function: "ts.string.substring",
            },
            Row::Method {
                path: "String.prototype.indexOf",
                length: 1,
                receivers: &[Receiver::Text],
                function: "ts.string.indexOf",
            },
            Row::Method {
                path: "String.prototype.lastIndexOf",
                length: 1,
                receivers: &[Receiver::Text],
                function: "ts.string.lastIndexOf",
            },
            Row::Method {
                path: "String.prototype.includes",
                length: 1,
                receivers: &[Receiver::Text],
                function: "ts.string.includes",
            },
            Row::Method {
                path: "String.prototype.startsWith",
                length: 1,
                receivers: &[Receiver::Text],
                function: "ts.string.startsWith",
            },
            Row::Method {
                path: "String.prototype.endsWith",
                length: 1,
                receivers: &[Receiver::Text],
                function: "ts.string.endsWith",
            },
            Row::Method {
                path: "String.prototype.concat",
                length: 1,
                receivers: &[Receiver::Text],
                function: "ts.string.concat",
            },
            Row::Method {
                path: "String.prototype.toLowerCase",
                length: 0,
                receivers: &[Receiver::Text],
                function: "ts.string.toLowerCase",
            },
            Row::Method {
                path: "String.prototype.toUpperCase",
                length: 0,
                receivers: &[Receiver::Text],
                function: "ts.string.toUpperCase",
            },
            Row::Method {
                path: "String.prototype.trim",
                length: 0,
                receivers: &[Receiver::Text],
                function: "ts.string.trim",
            },
            Row::Method {
                path: "String.prototype.trimStart",
                length: 0,
                receivers: &[Receiver::Text],
                function: "ts.string.trimStart",
            },
            Row::Method {
                path: "String.prototype.trimEnd",
                length: 0,
                receivers: &[Receiver::Text],
                function: "ts.string.trimEnd",
            },
            Row::Method {
                path: "String.prototype.repeat",
                length: 1,
                receivers: &[Receiver::Text],
                function: "ts.string.repeat",
            },
            Row::Method {
                path: "String.prototype.padStart",
                length: 1,
                receivers: &[Receiver::Text],
                function: "ts.string.padStart",
            },
            Row::Method {
                path: "String.prototype.padEnd",
                length: 1,
                receivers: &[Receiver::Text],
                function: "ts.string.padEnd",
            },
            Row::Method {
                path: "String.prototype.toString",
                length: 0,
                receivers: &[Receiver::Text],
                function: "ts.string.toString",
            },
            Row::Method {
                path: "String.prototype.valueOf",
                length: 0,
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
                length: 1,
                function: "ts.string.create",
            },
            Row::Function {
                path: "String.fromCharCode",
                length: 1,
                function: "ts.string.fromCharCode",
            },
            Row::Function {
                path: "String.fromCodePoint",
                length: 1,
                function: "ts.string.fromCodePoint",
            },
            Row::Function {
                path: "String.raw",
                length: 1,
                function: "ts.string.raw",
            },
        ],
    }
}
