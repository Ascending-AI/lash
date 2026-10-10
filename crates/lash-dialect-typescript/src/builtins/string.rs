use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/string.kernel"),
        rows: vec![
            Row::Function {
                path: "String.prototype.normalize",
                function: "ts.string.normalize",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "normalize",
                function: "ts.string.normalize",
            },
            Row::Function {
                path: "String.prototype.at",
                function: "ts.string.at",
            },
            Row::Function {
                path: "String.prototype.charAt",
                function: "ts.string.charAt",
            },
            Row::Function {
                path: "String.prototype.charCodeAt",
                function: "ts.string.charCodeAt",
            },
            Row::Function {
                path: "String.prototype.codePointAt",
                function: "ts.string.codePointAt",
            },
            Row::Function {
                path: "String.prototype.slice",
                function: "ts.string.slice",
            },
            Row::Function {
                path: "String.prototype.substring",
                function: "ts.string.substring",
            },
            Row::Function {
                path: "String.prototype.indexOf",
                function: "ts.string.indexOf",
            },
            Row::Function {
                path: "String.prototype.lastIndexOf",
                function: "ts.string.lastIndexOf",
            },
            Row::Function {
                path: "String.prototype.includes",
                function: "ts.string.includes",
            },
            Row::Function {
                path: "String.prototype.startsWith",
                function: "ts.string.startsWith",
            },
            Row::Function {
                path: "String.prototype.endsWith",
                function: "ts.string.endsWith",
            },
            Row::Function {
                path: "String.prototype.concat",
                function: "ts.string.concat",
            },
            Row::Function {
                path: "String.prototype.toLowerCase",
                function: "ts.string.toLowerCase",
            },
            Row::Function {
                path: "String.prototype.toUpperCase",
                function: "ts.string.toUpperCase",
            },
            Row::Function {
                path: "String.prototype.trim",
                function: "ts.string.trim",
            },
            Row::Function {
                path: "String.prototype.trimStart",
                function: "ts.string.trimStart",
            },
            Row::Function {
                path: "String.prototype.trimEnd",
                function: "ts.string.trimEnd",
            },
            Row::Function {
                path: "String.prototype.repeat",
                function: "ts.string.repeat",
            },
            Row::Function {
                path: "String.prototype.padStart",
                function: "ts.string.padStart",
            },
            Row::Function {
                path: "String.prototype.padEnd",
                function: "ts.string.padEnd",
            },
            Row::Function {
                path: "String.prototype.toString",
                function: "ts.string.toString",
            },
            Row::Function {
                path: "String.prototype.valueOf",
                function: "ts.string.valueOf",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "at",
                function: "ts.string.at",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "charAt",
                function: "ts.string.charAt",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "charCodeAt",
                function: "ts.string.charCodeAt",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "codePointAt",
                function: "ts.string.codePointAt",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "slice",
                function: "ts.string.slice",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "substring",
                function: "ts.string.substring",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "indexOf",
                function: "ts.string.indexOf",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "lastIndexOf",
                function: "ts.string.lastIndexOf",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "includes",
                function: "ts.string.includes",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "startsWith",
                function: "ts.string.startsWith",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "endsWith",
                function: "ts.string.endsWith",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "concat",
                function: "ts.string.concat",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "toLowerCase",
                function: "ts.string.toLowerCase",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "toUpperCase",
                function: "ts.string.toUpperCase",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "trim",
                function: "ts.string.trim",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "trimStart",
                function: "ts.string.trimStart",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "trimEnd",
                function: "ts.string.trimEnd",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "repeat",
                function: "ts.string.repeat",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "padStart",
                function: "ts.string.padStart",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "padEnd",
                function: "ts.string.padEnd",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "toString",
                function: "ts.string.toString",
            },
            Row::Method {
                receiver: Receiver::Text,
                name: "valueOf",
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
