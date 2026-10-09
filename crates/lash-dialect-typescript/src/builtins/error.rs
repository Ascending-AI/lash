use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/error.kernel"),
        rows: vec![
            Row::Constructor {
                class: "Error",
                function: "ts.error.new_Error",
            },
            Row::Function {
                path: "Error",
                function: "ts.error.new_Error",
            },
            Row::InstanceOf {
                class: "Error",
                function: "ts.error.is_Error",
            },
            Row::Constructor {
                class: "TypeError",
                function: "ts.error.new_TypeError",
            },
            Row::Function {
                path: "TypeError",
                function: "ts.error.new_TypeError",
            },
            Row::InstanceOf {
                class: "TypeError",
                function: "ts.error.is_TypeError",
            },
            Row::Constructor {
                class: "RangeError",
                function: "ts.error.new_RangeError",
            },
            Row::Function {
                path: "RangeError",
                function: "ts.error.new_RangeError",
            },
            Row::InstanceOf {
                class: "RangeError",
                function: "ts.error.is_RangeError",
            },
            Row::Constructor {
                class: "SyntaxError",
                function: "ts.error.new_SyntaxError",
            },
            Row::Function {
                path: "SyntaxError",
                function: "ts.error.new_SyntaxError",
            },
            Row::InstanceOf {
                class: "SyntaxError",
                function: "ts.error.is_SyntaxError",
            },
            Row::Constructor {
                class: "ReferenceError",
                function: "ts.error.new_ReferenceError",
            },
            Row::Function {
                path: "ReferenceError",
                function: "ts.error.new_ReferenceError",
            },
            Row::InstanceOf {
                class: "ReferenceError",
                function: "ts.error.is_ReferenceError",
            },
            Row::Constructor {
                class: "URIError",
                function: "ts.error.new_URIError",
            },
            Row::Function {
                path: "URIError",
                function: "ts.error.new_URIError",
            },
            Row::InstanceOf {
                class: "URIError",
                function: "ts.error.is_URIError",
            },
            Row::Constructor {
                class: "EvalError",
                function: "ts.error.new_EvalError",
            },
            Row::Function {
                path: "EvalError",
                function: "ts.error.new_EvalError",
            },
            Row::InstanceOf {
                class: "EvalError",
                function: "ts.error.is_EvalError",
            },
            Row::Constructor {
                class: "AggregateError",
                function: "ts.error.new_AggregateError",
            },
            Row::Function {
                path: "AggregateError",
                function: "ts.error.new_AggregateError",
            },
            Row::InstanceOf {
                class: "AggregateError",
                function: "ts.error.is_AggregateError",
            },
            Row::Method {
                receiver: Receiver::Error,
                name: "toString",
                function: "ts.error.toString",
            },
            Row::Function {
                path: "Error.prototype.toString",
                function: "ts.error.toString",
            },
        ],
    }
}
