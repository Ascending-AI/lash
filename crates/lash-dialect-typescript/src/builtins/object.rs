use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/object.kernel"),
        rows: vec![
            Row::Method {
                receiver: Receiver::Record,
                name: "hasOwnProperty",
                function: "ts.object.hasOwnProperty",
            },
            Row::Function {
                path: "Object.prototype.hasOwnProperty",
                function: "ts.object.hasOwnProperty",
            },
            Row::Method {
                receiver: Receiver::Record,
                name: "toString",
                function: "ts.object.toString",
            },
            Row::Function {
                path: "Object.prototype.toString",
                function: "ts.object.toString",
            },
            Row::Method {
                receiver: Receiver::Record,
                name: "valueOf",
                function: "ts.object.valueOf",
            },
            Row::Function {
                path: "Object.prototype.valueOf",
                function: "ts.object.valueOf",
            },
            Row::Function {
                path: "Object",
                function: "ts.object.convert",
            },
            Row::Function {
                path: "Object.keys",
                function: "ts.object.keys",
            },
            Row::Function {
                path: "Object.values",
                function: "ts.object.values",
            },
            Row::Function {
                path: "Object.entries",
                function: "ts.object.entries",
            },
            Row::Function {
                path: "Object.fromEntries",
                function: "ts.object.fromEntries",
            },
            Row::Function {
                path: "Object.assign",
                function: "ts.object.assign",
            },
            Row::Function {
                path: "Object.hasOwn",
                function: "ts.object.hasOwn",
            },
            Row::Function {
                path: "Object.is",
                function: "ts.object.is",
            },
            Row::Function {
                path: "Object.groupBy",
                function: "ts.object.groupBy",
            },
            Row::Function {
                path: "Object.defineProperty",
                function: "ts.object.defineProperty",
            },
            Row::Function {
                path: "Object.defineProperties",
                function: "ts.object.defineProperties",
            },
            Row::Function {
                path: "Object.getOwnPropertyDescriptor",
                function: "ts.object.getOwnPropertyDescriptor",
            },
            Row::Function {
                path: "Object.getOwnPropertyDescriptors",
                function: "ts.object.getOwnPropertyDescriptors",
            },
            Row::Function {
                path: "Object.getOwnPropertyNames",
                function: "ts.object.getOwnPropertyNames",
            },
            Row::Function {
                path: "Object.getOwnPropertySymbols",
                function: "ts.object.getOwnPropertySymbols",
            },
            Row::Function {
                path: "Object.create",
                function: "ts.object.create",
            },
            Row::Function {
                path: "Object.getPrototypeOf",
                function: "ts.object.getPrototypeOf",
            },
            Row::Function {
                path: "Object.setPrototypeOf",
                function: "ts.object.setPrototypeOf",
            },
            Row::Function {
                path: "Object.freeze",
                function: "ts.object.freeze",
            },
            Row::Function {
                path: "Object.seal",
                function: "ts.object.seal",
            },
            Row::Function {
                path: "Object.preventExtensions",
                function: "ts.object.preventExtensions",
            },
            Row::Function {
                path: "Object.isFrozen",
                function: "ts.object.isFrozen",
            },
            Row::Function {
                path: "Object.isSealed",
                function: "ts.object.isSealed",
            },
            Row::Function {
                path: "Object.isExtensible",
                function: "ts.object.isExtensible",
            },
        ],
    }
}
