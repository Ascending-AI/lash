use super::{Object, Receiver, Row};

/// `Promise`: its static functions and the three methods of a promise.
/// There is no constructor row: a promise is settled by a task's end, and
/// `new Promise(executor)` would need a task something else can end.
pub(super) fn object() -> Object {
    let mut rows = vec![
        Row::Function {
            path: "Promise.resolve",
            function: "ts.promise.resolve",
        },
        Row::Function {
            path: "Promise.reject",
            function: "ts.promise.reject",
        },
        Row::Function {
            path: "Promise.all",
            function: "ts.promise.all",
        },
        Row::Function {
            path: "Promise.allSettled",
            function: "ts.promise.all_settled",
        },
        Row::Function {
            path: "Promise.race",
            function: "ts.promise.race",
        },
        Row::Function {
            path: "Promise.any",
            function: "ts.promise.any",
        },
        Row::InstanceOf {
            class: "Promise",
            function: "ts.promise.is",
        },
    ];
    rows.extend(
        [
            ("Promise.prototype.then", "ts.promise.then"),
            ("Promise.prototype.catch", "ts.promise.catch"),
            ("Promise.prototype.finally", "ts.promise.finally"),
        ]
        .map(|(path, function)| Row::Method {
            path,
            receivers: &[Receiver::Brand("Promise")],
            function,
        }),
    );
    Object {
        source: include_str!("../helpers/promise.kernel"),
        rows,
    }
}
