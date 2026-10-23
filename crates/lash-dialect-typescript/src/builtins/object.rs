use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/object.kernel"),
        rows: vec![
            Row::InstanceOf {
                class: "Object",
                function: "ts.object.is_instance",
            },
            Row::Method {
                path: "Object.prototype.hasOwnProperty",
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
                receivers: &[Receiver::Brand("typescript.arguments"), Receiver::Record],
                function: "ts.object.toString",
            },
            Row::Method {
                path: "Object.prototype.valueOf",
                receivers: &[Receiver::Brand("typescript.arguments"), Receiver::Record],
                function: "ts.object.valueOf",
            },
            Row::Method {
                path: "Object.prototype.propertyIsEnumerable",
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
                receivers: &[Receiver::Record, Receiver::Text, Receiver::Bool],
                function: "ts.object.toLocaleString",
            },
            Row::Method {
                path: "Object.prototype.isPrototypeOf",
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
