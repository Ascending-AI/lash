//! How a name the source does not bind reaches a library function.
//!
//! Each built-in object has one file here that lists its rows, and one
//! helper source in `helpers/` that defines the functions the rows name.
//! Every function a row names has the dialect's calling convention,
//! `(this: Any, args: List(Any)) -> Any`.
//!
//! The lowerer resolves a global path (`Math.max`, `new Map`) to its row's
//! function when it lowers. A member's receiver is not known then, so for
//! each method or property name that has rows the package builder generates
//! one dispatcher, `ts.method.<name>` or `ts.property.<name>`, which tests
//! the receiver's kind in row order and otherwise falls back to the
//! object's own property. Lanes that add rows for one name never edit one
//! file.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

mod console;

/// What a member's receiver is, as `ts.receiver` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Receiver {
    List,
    Text,
    Number,
    Bool,
    Map,
    Set,
    Error,
    Timestamp,
    Closure,
    /// A record whose `brand` field is this text.
    Brand(&'static str),
}

impl Receiver {
    /// The text `ts.receiver` gives a value of this kind.
    pub(crate) fn tag(self) -> String {
        match self {
            Self::List => "list".into(),
            Self::Text => "text".into(),
            Self::Number => "number".into(),
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
    /// `receiver.name(args)`.
    Method {
        receiver: Receiver,
        name: &'static str,
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
        function: &'static str,
    },
    /// `value instanceof Class`: the function is called with the value as
    /// its one argument and gives a bool.
    InstanceOf {
        class: &'static str,
        function: &'static str,
    },
}

/// A built-in object's helper source and its rows.
pub(crate) struct Object {
    pub(crate) source: &'static str,
    pub(crate) rows: Vec<Row>,
}

/// Every built-in object, in the order their helpers are defined: an object
/// may call the helpers of those before it.
pub(crate) fn objects() -> Vec<Object> {
    vec![console::object()]
}

/// The rows, indexed the ways the lowerer and the package builder ask.
#[derive(Debug, Default)]
pub(crate) struct Table {
    pub(crate) methods: BTreeMap<&'static str, Vec<(Receiver, &'static str)>>,
    pub(crate) properties: BTreeMap<&'static str, Vec<(Receiver, &'static str)>>,
    pub(crate) functions: BTreeMap<&'static str, &'static str>,
    pub(crate) values: BTreeMap<&'static str, &'static str>,
    pub(crate) constructors: BTreeMap<&'static str, &'static str>,
    pub(crate) instance_tests: BTreeMap<&'static str, &'static str>,
    /// The first segment of every global path and every class.
    globals: BTreeSet<&'static str>,
}

impl Table {
    fn build() -> Self {
        let mut table = Self::default();
        for row in objects().into_iter().flat_map(|object| object.rows) {
            let taken = match row {
                Row::Method {
                    receiver,
                    name,
                    function,
                } => insert_member(&mut table.methods, receiver, name, function),
                Row::Property {
                    receiver,
                    name,
                    function,
                } => insert_member(&mut table.properties, receiver, name, function),
                Row::Function { path, function } => {
                    table.globals.insert(root(path));
                    table.functions.insert(path, function).is_some()
                }
                Row::Value { path, function } => {
                    table.globals.insert(root(path));
                    table.values.insert(path, function).is_some()
                }
                Row::Constructor { class, function } => {
                    table.globals.insert(class);
                    table.constructors.insert(class, function).is_some()
                }
                Row::InstanceOf { class, function } => {
                    table.globals.insert(class);
                    table.instance_tests.insert(class, function).is_some()
                }
            };
            assert!(
                !taken,
                "two built-in rows answer the same question: {row:?}"
            );
        }
        table
    }
}

fn insert_member(
    members: &mut BTreeMap<&'static str, Vec<(Receiver, &'static str)>>,
    receiver: Receiver,
    name: &'static str,
    function: &'static str,
) -> bool {
    let rows = members.entry(name).or_default();
    let taken = rows.iter().any(|(existing, _)| *existing == receiver);
    rows.push((receiver, function));
    taken
}

fn root(path: &'static str) -> &'static str {
    path.split('.').next().unwrap_or(path)
}

pub(crate) fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(Table::build)
}

/// Whether `name` is a global a built-in row answers for: a namespace
/// (`Math`), a function (`parseInt`) or a class (`Map`).
pub(crate) fn is_global(name: &str) -> bool {
    table().globals.contains(name)
        || matches!(name, "globalThis" | "undefined" | "NaN" | "Infinity")
}
