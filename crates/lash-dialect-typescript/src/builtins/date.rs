use super::{Object, Receiver, Row};

pub(super) fn object() -> Object {
    Object {
        source: include_str!("../helpers/date.kernel"),
        rows: vec![
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
                receiver: Receiver::Timestamp,
                name: "getTime",
                function: "ts.date.getTime",
            },
            Row::Function {
                path: "Date.prototype.getTime",
                function: "ts.date.getTime",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "valueOf",
                function: "ts.date.valueOf",
            },
            Row::Function {
                path: "Date.prototype.valueOf",
                function: "ts.date.valueOf",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "getUTCFullYear",
                function: "ts.date.getUTCFullYear",
            },
            Row::Function {
                path: "Date.prototype.getUTCFullYear",
                function: "ts.date.getUTCFullYear",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "getUTCMonth",
                function: "ts.date.getUTCMonth",
            },
            Row::Function {
                path: "Date.prototype.getUTCMonth",
                function: "ts.date.getUTCMonth",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "getUTCDate",
                function: "ts.date.getUTCDate",
            },
            Row::Function {
                path: "Date.prototype.getUTCDate",
                function: "ts.date.getUTCDate",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "getUTCDay",
                function: "ts.date.getUTCDay",
            },
            Row::Function {
                path: "Date.prototype.getUTCDay",
                function: "ts.date.getUTCDay",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "getUTCHours",
                function: "ts.date.getUTCHours",
            },
            Row::Function {
                path: "Date.prototype.getUTCHours",
                function: "ts.date.getUTCHours",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "getUTCMinutes",
                function: "ts.date.getUTCMinutes",
            },
            Row::Function {
                path: "Date.prototype.getUTCMinutes",
                function: "ts.date.getUTCMinutes",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "getUTCSeconds",
                function: "ts.date.getUTCSeconds",
            },
            Row::Function {
                path: "Date.prototype.getUTCSeconds",
                function: "ts.date.getUTCSeconds",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "getUTCMilliseconds",
                function: "ts.date.getUTCMilliseconds",
            },
            Row::Function {
                path: "Date.prototype.getUTCMilliseconds",
                function: "ts.date.getUTCMilliseconds",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "toISOString",
                function: "ts.date.toISOString",
            },
            Row::Function {
                path: "Date.prototype.toISOString",
                function: "ts.date.toISOString",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "toUTCString",
                function: "ts.date.toUTCString",
            },
            Row::Function {
                path: "Date.prototype.toUTCString",
                function: "ts.date.toUTCString",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "toString",
                function: "ts.date.toString",
            },
            Row::Function {
                path: "Date.prototype.toString",
                function: "ts.date.toString",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "toJSON",
                function: "ts.date.toJSON",
            },
            Row::Function {
                path: "Date.prototype.toJSON",
                function: "ts.date.toJSON",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "getFullYear",
                function: "ts.date.getFullYear",
            },
            Row::Function {
                path: "Date.prototype.getFullYear",
                function: "ts.date.getFullYear",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "getMonth",
                function: "ts.date.getMonth",
            },
            Row::Function {
                path: "Date.prototype.getMonth",
                function: "ts.date.getMonth",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "getDate",
                function: "ts.date.getDate",
            },
            Row::Function {
                path: "Date.prototype.getDate",
                function: "ts.date.getDate",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "getDay",
                function: "ts.date.getDay",
            },
            Row::Function {
                path: "Date.prototype.getDay",
                function: "ts.date.getDay",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "getHours",
                function: "ts.date.getHours",
            },
            Row::Function {
                path: "Date.prototype.getHours",
                function: "ts.date.getHours",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "getMinutes",
                function: "ts.date.getMinutes",
            },
            Row::Function {
                path: "Date.prototype.getMinutes",
                function: "ts.date.getMinutes",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "getSeconds",
                function: "ts.date.getSeconds",
            },
            Row::Function {
                path: "Date.prototype.getSeconds",
                function: "ts.date.getSeconds",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "getMilliseconds",
                function: "ts.date.getMilliseconds",
            },
            Row::Function {
                path: "Date.prototype.getMilliseconds",
                function: "ts.date.getMilliseconds",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "setTime",
                function: "ts.date.setTime",
            },
            Row::Function {
                path: "Date.prototype.setTime",
                function: "ts.date.setTime",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "setFullYear",
                function: "ts.date.setFullYear",
            },
            Row::Function {
                path: "Date.prototype.setFullYear",
                function: "ts.date.setFullYear",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "setMonth",
                function: "ts.date.setMonth",
            },
            Row::Function {
                path: "Date.prototype.setMonth",
                function: "ts.date.setMonth",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "setDate",
                function: "ts.date.setDate",
            },
            Row::Function {
                path: "Date.prototype.setDate",
                function: "ts.date.setDate",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "setHours",
                function: "ts.date.setHours",
            },
            Row::Function {
                path: "Date.prototype.setHours",
                function: "ts.date.setHours",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "setMinutes",
                function: "ts.date.setMinutes",
            },
            Row::Function {
                path: "Date.prototype.setMinutes",
                function: "ts.date.setMinutes",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "setSeconds",
                function: "ts.date.setSeconds",
            },
            Row::Function {
                path: "Date.prototype.setSeconds",
                function: "ts.date.setSeconds",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "setMilliseconds",
                function: "ts.date.setMilliseconds",
            },
            Row::Function {
                path: "Date.prototype.setMilliseconds",
                function: "ts.date.setMilliseconds",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "setUTCFullYear",
                function: "ts.date.setUTCFullYear",
            },
            Row::Function {
                path: "Date.prototype.setUTCFullYear",
                function: "ts.date.setUTCFullYear",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "setUTCMonth",
                function: "ts.date.setUTCMonth",
            },
            Row::Function {
                path: "Date.prototype.setUTCMonth",
                function: "ts.date.setUTCMonth",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "setUTCDate",
                function: "ts.date.setUTCDate",
            },
            Row::Function {
                path: "Date.prototype.setUTCDate",
                function: "ts.date.setUTCDate",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "setUTCHours",
                function: "ts.date.setUTCHours",
            },
            Row::Function {
                path: "Date.prototype.setUTCHours",
                function: "ts.date.setUTCHours",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "setUTCMinutes",
                function: "ts.date.setUTCMinutes",
            },
            Row::Function {
                path: "Date.prototype.setUTCMinutes",
                function: "ts.date.setUTCMinutes",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "setUTCSeconds",
                function: "ts.date.setUTCSeconds",
            },
            Row::Function {
                path: "Date.prototype.setUTCSeconds",
                function: "ts.date.setUTCSeconds",
            },
            Row::Method {
                receiver: Receiver::Timestamp,
                name: "setUTCMilliseconds",
                function: "ts.date.setUTCMilliseconds",
            },
            Row::Function {
                path: "Date.prototype.setUTCMilliseconds",
                function: "ts.date.setUTCMilliseconds",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "getTime",
                function: "ts.date.getTime",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "valueOf",
                function: "ts.date.valueOf",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "getUTCFullYear",
                function: "ts.date.getUTCFullYear",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "getUTCMonth",
                function: "ts.date.getUTCMonth",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "getUTCDate",
                function: "ts.date.getUTCDate",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "getUTCDay",
                function: "ts.date.getUTCDay",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "getUTCHours",
                function: "ts.date.getUTCHours",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "getUTCMinutes",
                function: "ts.date.getUTCMinutes",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "getUTCSeconds",
                function: "ts.date.getUTCSeconds",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "getUTCMilliseconds",
                function: "ts.date.getUTCMilliseconds",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "toISOString",
                function: "ts.date.toISOString",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "toUTCString",
                function: "ts.date.toUTCString",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "toString",
                function: "ts.date.toString",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "toJSON",
                function: "ts.date.toJSON",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "getFullYear",
                function: "ts.date.getFullYear",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "getMonth",
                function: "ts.date.getMonth",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "getDate",
                function: "ts.date.getDate",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "getDay",
                function: "ts.date.getDay",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "getHours",
                function: "ts.date.getHours",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "getMinutes",
                function: "ts.date.getMinutes",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "getSeconds",
                function: "ts.date.getSeconds",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "getMilliseconds",
                function: "ts.date.getMilliseconds",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "setTime",
                function: "ts.date.setTime",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "setFullYear",
                function: "ts.date.setFullYear",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "setMonth",
                function: "ts.date.setMonth",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "setDate",
                function: "ts.date.setDate",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "setHours",
                function: "ts.date.setHours",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "setMinutes",
                function: "ts.date.setMinutes",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "setSeconds",
                function: "ts.date.setSeconds",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "setMilliseconds",
                function: "ts.date.setMilliseconds",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "setUTCFullYear",
                function: "ts.date.setUTCFullYear",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "setUTCMonth",
                function: "ts.date.setUTCMonth",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "setUTCDate",
                function: "ts.date.setUTCDate",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "setUTCHours",
                function: "ts.date.setUTCHours",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "setUTCMinutes",
                function: "ts.date.setUTCMinutes",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "setUTCSeconds",
                function: "ts.date.setUTCSeconds",
            },
            Row::Method {
                receiver: Receiver::Brand("date.invalid"),
                name: "setUTCMilliseconds",
                function: "ts.date.setUTCMilliseconds",
            },
        ],
    }
}
