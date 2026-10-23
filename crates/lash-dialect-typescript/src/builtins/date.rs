use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/date.kernel"),
        rows: vec![
            Row::Method {
                path: "Date.prototype.toLocaleString",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.toLocaleString",
            },
            Row::Method {
                path: "Date.prototype.getTimezoneOffset",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.local_unsupported",
            },
            Row::Method {
                path: "Date.prototype.toDateString",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.local_unsupported",
            },
            Row::Method {
                path: "Date.prototype.toLocaleDateString",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.local_unsupported",
            },
            Row::Method {
                path: "Date.prototype.toLocaleTimeString",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.local_unsupported",
            },
            Row::Method {
                path: "Date.prototype.toTimeString",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.local_unsupported",
            },
            Row::Constructor {
                class: "Date",
                function: "ts.date.construct",
            },
            Row::InstanceOf {
                class: "Date",
                function: "ts.date.is",
            },
            Row::Function {
                path: "Date",
                function: "ts.date.call",
            },
            Row::Function {
                path: "Date.now",
                function: "ts.date.now",
            },
            Row::Function {
                path: "Date.parse",
                function: "ts.date.parse",
            },
            Row::Function {
                path: "Date.UTC",
                function: "ts.date.UTC",
            },
            Row::Method {
                path: "Date.prototype.getTime",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getTime",
            },
            Row::Method {
                path: "Date.prototype.valueOf",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.valueOf",
            },
            Row::Method {
                path: "Date.prototype.getUTCFullYear",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getUTCFullYear",
            },
            Row::Method {
                path: "Date.prototype.getUTCMonth",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getUTCMonth",
            },
            Row::Method {
                path: "Date.prototype.getUTCDate",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getUTCDate",
            },
            Row::Method {
                path: "Date.prototype.getUTCDay",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getUTCDay",
            },
            Row::Method {
                path: "Date.prototype.getUTCHours",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getUTCHours",
            },
            Row::Method {
                path: "Date.prototype.getUTCMinutes",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getUTCMinutes",
            },
            Row::Method {
                path: "Date.prototype.getUTCSeconds",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getUTCSeconds",
            },
            Row::Method {
                path: "Date.prototype.getUTCMilliseconds",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getUTCMilliseconds",
            },
            Row::Method {
                path: "Date.prototype.toISOString",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.toISOString",
            },
            Row::Method {
                path: "Date.prototype.toUTCString",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.toUTCString",
            },
            Row::Method {
                path: "Date.prototype.toString",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.toString",
            },
            Row::Method {
                path: "Date.prototype.toJSON",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.toJSON",
            },
            Row::Method {
                path: "Date.prototype.getFullYear",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getFullYear",
            },
            Row::Method {
                path: "Date.prototype.getMonth",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getMonth",
            },
            Row::Method {
                path: "Date.prototype.getDate",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getDate",
            },
            Row::Method {
                path: "Date.prototype.getDay",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getDay",
            },
            Row::Method {
                path: "Date.prototype.getHours",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getHours",
            },
            Row::Method {
                path: "Date.prototype.getMinutes",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getMinutes",
            },
            Row::Method {
                path: "Date.prototype.getSeconds",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getSeconds",
            },
            Row::Method {
                path: "Date.prototype.getMilliseconds",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getMilliseconds",
            },
            Row::Method {
                path: "Date.prototype.setTime",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setTime",
            },
            Row::Method {
                path: "Date.prototype.setFullYear",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setFullYear",
            },
            Row::Method {
                path: "Date.prototype.setMonth",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setMonth",
            },
            Row::Method {
                path: "Date.prototype.setDate",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setDate",
            },
            Row::Method {
                path: "Date.prototype.setHours",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setHours",
            },
            Row::Method {
                path: "Date.prototype.setMinutes",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setMinutes",
            },
            Row::Method {
                path: "Date.prototype.setSeconds",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setSeconds",
            },
            Row::Method {
                path: "Date.prototype.setMilliseconds",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setMilliseconds",
            },
            Row::Method {
                path: "Date.prototype.setUTCFullYear",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setUTCFullYear",
            },
            Row::Method {
                path: "Date.prototype.setUTCMonth",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setUTCMonth",
            },
            Row::Method {
                path: "Date.prototype.setUTCDate",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setUTCDate",
            },
            Row::Method {
                path: "Date.prototype.setUTCHours",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setUTCHours",
            },
            Row::Method {
                path: "Date.prototype.setUTCMinutes",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setUTCMinutes",
            },
            Row::Method {
                path: "Date.prototype.setUTCSeconds",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setUTCSeconds",
            },
            Row::Method {
                path: "Date.prototype.setUTCMilliseconds",
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setUTCMilliseconds",
            },
        ],
    }
}
