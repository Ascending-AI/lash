use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/object.kernel"),
        rows: vec![
            Row::Constructor {
                class: "Object",
                length: None,
                function: "ts.object.convert",
            },
            Row::InstanceOf {
                class: "Object",
                function: "ts.object.is_instance",
            },
            Row::Method {
                path: "Object.prototype.hasOwnProperty",
                length: 1,
                receivers: &[
                    Receiver::Brand("typescript.arguments"),
                    Receiver::Record,
                    Receiver::List,
                    Receiver::Text,
                    Receiver::Number,
                    Receiver::Bool,
                    Receiver::Map,
                    Receiver::Set,
                    Receiver::Error,
                    Receiver::Timestamp,
                    Receiver::Closure,
                ],
                function: "ts.object.hasOwnProperty",
            },
            Row::Method {
                path: "Object.prototype.toString",
                length: 0,
                receivers: &[Receiver::Brand("typescript.arguments"), Receiver::Record],
                function: "ts.object.toString",
            },
            Row::Method {
                path: "Object.prototype.valueOf",
                length: 0,
                receivers: &[Receiver::Brand("typescript.arguments"), Receiver::Record],
                function: "ts.object.valueOf",
            },
            Row::Method {
                path: "Object.prototype.propertyIsEnumerable",
                length: 1,
                receivers: &[
                    Receiver::Record,
                    Receiver::List,
                    Receiver::Text,
                    Receiver::Number,
                    Receiver::Bool,
                    Receiver::Map,
                    Receiver::Set,
                    Receiver::Error,
                    Receiver::Timestamp,
                    Receiver::Closure,
                    Receiver::Brand("typescript.arguments"),
                ],
                function: "ts.object.propertyIsEnumerable",
            },
            Row::Method {
                path: "Object.prototype.toLocaleString",
                length: 0,
                receivers: &[Receiver::Record, Receiver::Text, Receiver::Bool],
                function: "ts.object.toLocaleString",
            },
            Row::Method {
                path: "Object.prototype.isPrototypeOf",
                length: 1,
                receivers: &[
                    Receiver::Record,
                    Receiver::List,
                    Receiver::Text,
                    Receiver::Number,
                    Receiver::Bool,
                    Receiver::Map,
                    Receiver::Set,
                    Receiver::Error,
                    Receiver::Timestamp,
                    Receiver::Closure,
                    Receiver::Brand("typescript.arguments"),
                ],
                function: "ts.object.isPrototypeOf",
            },
            Row::Function {
                path: "Reflect.get",
                length: 2,
                function: "ts.reflect.get",
            },
            Row::Function {
                path: "Reflect.has",
                length: 2,
                function: "ts.reflect.has",
            },
            Row::Function {
                path: "Reflect.getOwnPropertyDescriptor",
                length: 2,
                function: "ts.reflect.descriptor",
            },
            Row::Function {
                path: "Reflect.ownKeys",
                length: 1,
                function: "ts.reflect.ownKeys",
            },
            Row::Function {
                path: "Object",
                length: 1,
                function: "ts.object.convert",
            },
            Row::Function {
                path: "Object.keys",
                length: 1,
                function: "ts.object.keys",
            },
            Row::Function {
                path: "Object.values",
                length: 1,
                function: "ts.object.values",
            },
            Row::Function {
                path: "Object.entries",
                length: 1,
                function: "ts.object.entries",
            },
            Row::Function {
                path: "Object.fromEntries",
                length: 1,
                function: "ts.object.fromEntries",
            },
            Row::Function {
                path: "Object.assign",
                length: 2,
                function: "ts.object.assign",
            },
            Row::Function {
                path: "Object.hasOwn",
                length: 2,
                function: "ts.object.hasOwn",
            },
            Row::Function {
                path: "Object.is",
                length: 2,
                function: "ts.object.is",
            },
            Row::Function {
                path: "Object.groupBy",
                length: 2,
                function: "ts.object.groupBy",
            },
            Row::Function {
                path: "Object.defineProperty",
                length: 3,
                function: "ts.object.defineProperty",
            },
            Row::Function {
                path: "Object.defineProperties",
                length: 2,
                function: "ts.object.defineProperties",
            },
            Row::Function {
                path: "Object.getOwnPropertyDescriptor",
                length: 2,
                function: "ts.reflect.getOwnPropertyDescriptor",
            },
            Row::Function {
                path: "Object.getOwnPropertyDescriptors",
                length: 1,
                function: "ts.reflect.getOwnPropertyDescriptors",
            },
            Row::Function {
                path: "Object.getOwnPropertyNames",
                length: 1,
                function: "ts.reflect.getOwnPropertyNames",
            },
            Row::Function {
                path: "Object.getOwnPropertySymbols",
                length: 1,
                function: "ts.reflect.getOwnPropertySymbols",
            },
            Row::Function {
                path: "Object.create",
                length: 2,
                function: "ts.object.create",
            },
            Row::Function {
                path: "Object.getPrototypeOf",
                length: 1,
                function: "ts.object.getPrototypeOf",
            },
            Row::Function {
                path: "Object.setPrototypeOf",
                length: 2,
                function: "ts.object.setPrototypeOf",
            },
            Row::Function {
                path: "Object.freeze",
                length: 1,
                function: "ts.object.freeze",
            },
            Row::Function {
                path: "Object.seal",
                length: 1,
                function: "ts.object.seal",
            },
            Row::Function {
                path: "Object.preventExtensions",
                length: 1,
                function: "ts.object.preventExtensions",
            },
            Row::Function {
                path: "Object.isFrozen",
                length: 1,
                function: "ts.object.isFrozen",
            },
            Row::Function {
                path: "Object.isSealed",
                length: 1,
                function: "ts.object.isSealed",
            },
            Row::Function {
                path: "Object.isExtensible",
                length: 1,
                function: "ts.object.isExtensible",
            },
        ],
    }
}
