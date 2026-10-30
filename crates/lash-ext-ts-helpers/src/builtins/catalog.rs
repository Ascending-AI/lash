//! Built-in rows shared by the package builder and the build-time index generator.

#[path = "abort.rs"]
mod abort;
#[path = "array.rs"]
mod array;
#[path = "bigint.rs"]
mod bigint;
#[path = "boolean.rs"]
mod boolean;
#[path = "console.rs"]
mod console;
#[path = "date.rs"]
mod date;
#[path = "error.rs"]
mod error;
#[path = "function.rs"]
mod function;
#[path = "json.rs"]
mod json;
#[path = "map.rs"]
mod map;
#[path = "math.rs"]
mod math;
#[path = "number.rs"]
mod number;
#[path = "object.rs"]
mod object;
#[path = "promise.rs"]
mod promise;
#[path = "regexp.rs"]
mod regexp;
#[path = "set.rs"]
mod set;
#[path = "string.rs"]
mod string;
#[path = "uri.rs"]
mod uri;
#[path = "url.rs"]
mod url;
#[path = "url_search_params.rs"]
mod url_search_params;

/// What a member's receiver is, as `ts.receiver` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Receiver {
    List,
    Record,
    Text,
    Number,
    BigInt,
    Bool,
    Map,
    Set,
    Error,
    Timestamp,
    Closure,
    /// An internal record with the kernel tuple marker and this type tag.
    Brand(&'static str),
}

impl Receiver {
    /// The text `ts.receiver` gives a value of this kind.
    pub fn tag(self) -> String {
        match self {
            Self::Record => "record".into(),
            Self::List => "list".into(),
            Self::Text => "text".into(),
            Self::Number => "number".into(),
            Self::BigInt => "bigint".into(),
            Self::Bool => "bool".into(),
            Self::Map => "map".into(),
            Self::Set => "set".into(),
            Self::Error => "error".into(),
            Self::Timestamp => "timestamp".into(),
            Self::Closure => "closure".into(),
            Self::Brand(brand) => format!("brand:{brand}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// A prototype's method (`Array.prototype.map`): one function object,
    /// which a read of `path` gives, and which a value of each of
    /// `receivers` answers for the member named by the path's last segment,
    /// so `receiver.name(args)` calls it.
    Method {
        path: &'static str,
        /// JavaScript function length, before optional or rest parameters.
        length: u8,
        receivers: &'static [Receiver],
        function: &'static str,
    },
    /// A read of `receiver.name`: the function is called with the receiver
    /// alone.
    Property {
        receiver: Receiver,
        name: &'static str,
        function: &'static str,
    },
    /// A call of a global path: `Math.max(...)`, `parseInt(...)`.
    Function {
        path: &'static str,
        length: u8,
        function: &'static str,
    },
    /// A read of a global path: `Math.PI`. The function is called with no
    /// arguments.
    Value {
        path: &'static str,
        function: &'static str,
    },
    /// `new Class(args)`.
    Constructor {
        class: &'static str,
        /// Supplied here only when no Function row owns this class's length.
        length: Option<u8>,
        function: &'static str,
    },
    /// Class metadata when no function or constructor row owns its length.
    Class { class: &'static str, length: u8 },
    /// `value instanceof Class`: the function is called with the value as
    /// its one argument and gives a bool.
    InstanceOf {
        class: &'static str,
        function: &'static str,
    },
}

/// A built-in object's helper source and its rows.
pub struct Object {
    pub source: &'static str,
    pub rows: Vec<Row>,
}

/// Every built-in object, in the order their helpers are defined: an object
/// may call the helpers of those before it.
pub fn objects() -> Vec<Object> {
    vec![
        function::object(),
        map::object(),
        set::object(),
        boolean::object(),
        object::object(),
        math::object(),
        number::object(),
        bigint::object(),
        date::object(),
        error::object(),
        url::object(),
        url_search_params::object(),
        console::object(),
        json::object(),
        array::object(),
        promise::object(),
        abort::object(),
        string::object(),
        regexp::object(),
        uri::object(),
    ]
}

/// A built-in function a direct call reaches without its argument list:
/// its row's helper reads a fixed number of arguments, `undefined` for
/// each one missing, and answers what one helper answers over them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Positional {
    /// The helper the row's helper answers with.
    pub helper: &'static str,
    /// How many arguments it reads.
    pub arity: usize,
    /// The helper a first argument proven a float takes instead.
    pub float: Option<&'static str>,
    /// What a call with no arguments gives, where that is not the helper's
    /// answer for `undefined`.
    pub empty: Option<Empty>,
}

/// What a positional built-in called with no arguments gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Empty {
    /// `String()`.
    Text,
    /// `Number()`.
    Zero,
}
