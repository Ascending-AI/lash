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
            length: 0,
            function: match method {
                "log" => "ts.console.log",
                "warn" => "ts.console.warn",
                "error" => "ts.console.error",
                "info" => "ts.console.info",
                _ => "ts.console.debug",
            },
        })
        .collect();
    Object {
        source: include_str!("../helpers/console.kernel"),
        rows,
    }
}
