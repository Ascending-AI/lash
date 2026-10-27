use super::{Object, Receiver, Row};

/// `Promise`: its static functions and the three methods of a promise.
/// There is no constructor row: a promise is settled by a task's end, and
/// `new Promise(executor)` would need a task something else can end.
pub(super) fn object() -> Object {
    let mut rows = vec![
        Row::Class {
            class: "Promise",
            length: 1,
        },
        Row::Function {
            path: "Promise.resolve",
            length: 1,
            function: "ts.promise.resolve",
        },
        Row::Function {
            path: "Promise.reject",
            length: 1,
            function: "ts.promise.reject",
        },
        Row::Function {
            path: "Promise.all",
            length: 1,
            function: "ts.promise.all",
        },
        Row::Function {
            path: "Promise.allSettled",
            length: 1,
            function: "ts.promise.all_settled",
        },
        Row::Function {
            path: "Promise.race",
            length: 1,
            function: "ts.promise.race",
        },
        Row::Function {
            path: "Promise.any",
            length: 1,
            function: "ts.promise.any",
        },
        Row::InstanceOf {
            class: "Promise",
            function: "ts.promise.is",
        },
    ];
    rows.extend(
        [
            ("Promise.prototype.then", 2, "ts.promise.then"),
            ("Promise.prototype.catch", 1, "ts.promise.catch"),
            ("Promise.prototype.finally", 1, "ts.promise.finally"),
        ]
        .map(|(path, length, function)| Row::Method {
            path,
            length,
            receivers: &[Receiver::Brand("typescript.promise")],
            function,
        }),
    );
    Object {
        source: include_str!("../helpers/promise.kernel"),
        rows,
    }
}
