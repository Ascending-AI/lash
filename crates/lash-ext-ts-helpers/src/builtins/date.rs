use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/date.kernel"),
        rows: vec![
            Row::Method {
                path: "Date.prototype.toLocaleString",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.toLocaleString",
            },
            Row::Method {
                path: "Date.prototype.getTimezoneOffset",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getTimezoneOffset",
            },
            Row::Method {
                path: "Date.prototype.toDateString",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.toDateString",
            },
            Row::Method {
                path: "Date.prototype.toLocaleDateString",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.toLocaleDateString",
            },
            Row::Method {
                path: "Date.prototype.toLocaleTimeString",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.toLocaleTimeString",
            },
            Row::Method {
                path: "Date.prototype.toTimeString",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.toTimeString",
            },
            Row::Constructor {
                class: "Date",
                length: None,
                function: "ts.date.construct",
            },
            Row::InstanceOf {
                class: "Date",
                function: "ts.date.is",
            },
            Row::Function {
                path: "Date",
                length: 7,
                function: "ts.date.call",
            },
            Row::Function {
                path: "Date.now",
                length: 0,
                function: "ts.date.now",
            },
            Row::Function {
                path: "Date.parse",
                length: 1,
                function: "ts.date.parse",
            },
            Row::Function {
                path: "Date.UTC",
                length: 7,
                function: "ts.date.UTC",
            },
            Row::Method {
                path: "Date.prototype.getTime",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getTime",
            },
            Row::Method {
                path: "Date.prototype.valueOf",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.valueOf",
            },
            Row::Method {
                path: "Date.prototype.getUTCFullYear",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getUTCFullYear",
            },
            Row::Method {
                path: "Date.prototype.getUTCMonth",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getUTCMonth",
            },
            Row::Method {
                path: "Date.prototype.getUTCDate",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getUTCDate",
            },
            Row::Method {
                path: "Date.prototype.getUTCDay",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getUTCDay",
            },
            Row::Method {
                path: "Date.prototype.getUTCHours",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getUTCHours",
            },
            Row::Method {
                path: "Date.prototype.getUTCMinutes",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getUTCMinutes",
            },
            Row::Method {
                path: "Date.prototype.getUTCSeconds",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getUTCSeconds",
            },
            Row::Method {
                path: "Date.prototype.getUTCMilliseconds",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getUTCMilliseconds",
            },
            Row::Method {
                path: "Date.prototype.toISOString",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.toISOString",
            },
            Row::Method {
                path: "Date.prototype.toUTCString",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.toUTCString",
            },
            Row::Method {
                path: "Date.prototype.toString",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.toString",
            },
            Row::Method {
                path: "Date.prototype.toJSON",
                length: 1,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.toJSON",
            },
            Row::Method {
                path: "Date.prototype.getFullYear",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getFullYear",
            },
            Row::Method {
                path: "Date.prototype.getMonth",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getMonth",
            },
            Row::Method {
                path: "Date.prototype.getDate",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getDate",
            },
            Row::Method {
                path: "Date.prototype.getDay",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getDay",
            },
            Row::Method {
                path: "Date.prototype.getHours",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getHours",
            },
            Row::Method {
                path: "Date.prototype.getMinutes",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getMinutes",
            },
            Row::Method {
                path: "Date.prototype.getSeconds",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getSeconds",
            },
            Row::Method {
                path: "Date.prototype.getMilliseconds",
                length: 0,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.getMilliseconds",
            },
            Row::Method {
                path: "Date.prototype.setTime",
                length: 1,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setTime",
            },
            Row::Method {
                path: "Date.prototype.setFullYear",
                length: 3,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setFullYear",
            },
            Row::Method {
                path: "Date.prototype.setMonth",
                length: 2,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setMonth",
            },
            Row::Method {
                path: "Date.prototype.setDate",
                length: 1,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setDate",
            },
            Row::Method {
                path: "Date.prototype.setHours",
                length: 4,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setHours",
            },
            Row::Method {
                path: "Date.prototype.setMinutes",
                length: 3,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setMinutes",
            },
            Row::Method {
                path: "Date.prototype.setSeconds",
                length: 2,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setSeconds",
            },
            Row::Method {
                path: "Date.prototype.setMilliseconds",
                length: 1,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setMilliseconds",
            },
            Row::Method {
                path: "Date.prototype.setUTCFullYear",
                length: 3,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setUTCFullYear",
            },
            Row::Method {
                path: "Date.prototype.setUTCMonth",
                length: 2,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setUTCMonth",
            },
            Row::Method {
                path: "Date.prototype.setUTCDate",
                length: 1,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setUTCDate",
            },
            Row::Method {
                path: "Date.prototype.setUTCHours",
                length: 4,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setUTCHours",
            },
            Row::Method {
                path: "Date.prototype.setUTCMinutes",
                length: 3,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setUTCMinutes",
            },
            Row::Method {
                path: "Date.prototype.setUTCSeconds",
                length: 2,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setUTCSeconds",
            },
            Row::Method {
                path: "Date.prototype.setUTCMilliseconds",
                length: 1,
                receivers: &[
                    Receiver::Timestamp,
                    Receiver::Brand("date.ecma.invalid_date"),
                ],
                function: "ts.date.setUTCMilliseconds",
            },
        ],
    }
}
