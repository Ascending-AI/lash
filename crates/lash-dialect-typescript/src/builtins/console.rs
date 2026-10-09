use super::{Object, Row};

pub(super) fn object() -> Object {
    let rows = ["log", "warn", "error", "info", "debug"]
        .into_iter()
        .map(|method| Row::Function {
            path: match method {
                "log" => "console.log",
                "warn" => "console.warn",
                "error" => "console.error",
                "info" => "console.info",
                _ => "console.debug",
            },
            function: "ts.console.log",
        })
        .collect();
    Object {
        source: include_str!("../helpers/console.kernel"),
        rows,
    }
}
