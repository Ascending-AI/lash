//! Method facts and runtime attribute helpers shared by source lowering and the release builder.

#[derive(Clone, Copy, Debug)]
pub enum ResultKind {
    Bool,
    Dict,
    Int,
    List,
    None,
    Set,
    Str,
    Tuple,
    Unknown,
}

/// A method: its name, the fewest and the most arguments it takes, and the
/// names its arguments may be passed under, in order.
pub struct Method {
    pub name: &'static str,
    pub min: usize,
    pub packed: bool,
    /// Also expose an unbound descriptor on each admitted receiver type.
    pub descriptor: bool,
    pub kinds: &'static [&'static str],
    pub max: usize,
    pub positional: usize,
    pub keywords: &'static [&'static str],
    pub result: ResultKind,
}

const fn method(
    name: &'static str,
    min: usize,
    max: usize,
    result: ResultKind,
    kinds: &'static [&'static str],
) -> Method {
    Method {
        name,
        min,
        packed: false,
        descriptor: false,
        kinds,
        max,
        positional: max,
        keywords: &[],
        result,
    }
}

const fn packed_method(
    name: &'static str,
    result: ResultKind,
    kinds: &'static [&'static str],
) -> Method {
    let mut entry = method(name, 0, 1, result, kinds);
    entry.packed = true;
    entry
}

const fn descriptor_method(
    name: &'static str,
    min: usize,
    max: usize,
    result: ResultKind,
    kinds: &'static [&'static str],
) -> Method {
    Method {
        descriptor: true,
        ..method(name, min, max, result, kinds)
    }
}

pub const METHODS: &[Method] = &[
    method("append", 1, 1, ResultKind::None, &["list"]),
    method("extend", 1, 1, ResultKind::None, &["list"]),
    method("insert", 2, 2, ResultKind::None, &["list"]),
    method("pop", 0, 2, ResultKind::Unknown, &["list", "map", "set"]),
    method("remove", 1, 1, ResultKind::None, &["list", "set"]),
    method("index", 1, 3, ResultKind::Int, &["list", "tuple", "text"]),
    method("count", 1, 3, ResultKind::Int, &["list", "tuple", "text"]),
    Method {
        name: "sort",
        packed: false,
        descriptor: false,
        kinds: &["list"],
        min: 0,
        max: 2,
        positional: 2,
        keywords: &["key", "reverse"],
        result: ResultKind::None,
    },
    method("reverse", 0, 0, ResultKind::None, &["list"]),
    method("copy", 0, 0, ResultKind::Unknown, &["list", "map", "set"]),
    method("clear", 0, 0, ResultKind::None, &["list", "map", "set"]),
    method("get", 1, 2, ResultKind::Unknown, &["map"]),
    method("keys", 0, 0, ResultKind::List, &["map"]),
    method("values", 0, 0, ResultKind::List, &["map"]),
    method("items", 0, 0, ResultKind::List, &["map"]),
    method("setdefault", 1, 2, ResultKind::Unknown, &["map"]),
    packed_method("update", ResultKind::None, &["map", "set"]),
    method("add", 1, 1, ResultKind::None, &["set"]),
    method("discard", 1, 1, ResultKind::None, &["set"]),
    packed_method("union", ResultKind::Set, &["set"]),
    packed_method("intersection", ResultKind::Set, &["set"]),
    packed_method("difference", ResultKind::Set, &["set"]),
    method("format_map", 1, 1, ResultKind::Str, &["text"]),
    method("translate", 1, 1, ResultKind::Str, &["text"]),
    method("maketrans", 1, 3, ResultKind::Dict, &["text"]),
    packed_method("format", ResultKind::Str, &["text"]),
    method("popitem", 0, 0, ResultKind::Tuple, &["map"]),
    method("fromkeys", 1, 2, ResultKind::Dict, &["map"]),
    method(
        "conjugate",
        0,
        0,
        ResultKind::Unknown,
        &["integer", "float"],
    ),
    method("join", 1, 1, ResultKind::Str, &["text"]),
    Method {
        packed: false,
        descriptor: false,
        name: "split",
        kinds: &["text", "bytes"],
        min: 0,
        max: 2,
        positional: 2,
        keywords: &["sep", "maxsplit"],
        result: ResultKind::List,
    },
    method("strip", 0, 1, ResultKind::Str, &["text"]),
    method("lstrip", 0, 1, ResultKind::Str, &["text"]),
    method("rstrip", 0, 1, ResultKind::Str, &["text"]),
    method("upper", 0, 0, ResultKind::Str, &["text"]),
    method("lower", 0, 0, ResultKind::Str, &["text"]),
    method("startswith", 1, 3, ResultKind::Bool, &["text"]),
    method("endswith", 1, 3, ResultKind::Bool, &["text"]),
    Method {
        packed: false,
        descriptor: false,
        name: "replace",
        kinds: &["text"],
        min: 2,
        max: 3,
        positional: 3,
        keywords: &["", "", "count"],
        result: ResultKind::Str,
    },
    method("find", 1, 3, ResultKind::Int, &["text"]),
    method("title", 0, 0, ResultKind::Str, &["text"]),
    method("capitalize", 0, 0, ResultKind::Str, &["text"]),
    method("casefold", 0, 0, ResultKind::Str, &["text"]),
    method("swapcase", 0, 0, ResultKind::Str, &["text"]),
    descriptor_method("partition", 1, 1, ResultKind::Tuple, &["text", "bytes"]),
    descriptor_method("rpartition", 1, 1, ResultKind::Tuple, &["text", "bytes"]),
    method("center", 1, 2, ResultKind::Str, &["text"]),
    method("ljust", 1, 2, ResultKind::Str, &["text"]),
    method("rjust", 1, 2, ResultKind::Str, &["text"]),
    method("zfill", 1, 1, ResultKind::Str, &["text"]),
    Method {
        packed: false,
        descriptor: false,
        name: "expandtabs",
        kinds: &["text"],
        min: 0,
        max: 1,
        positional: 1,
        keywords: &["tabsize"],
        result: ResultKind::Str,
    },
    method("removeprefix", 1, 1, ResultKind::Str, &["text"]),
    method("removesuffix", 1, 1, ResultKind::Str, &["text"]),
    method("rfind", 1, 3, ResultKind::Int, &["text"]),
    method("rindex", 1, 3, ResultKind::Int, &["text"]),
    Method {
        packed: false,
        descriptor: false,
        name: "rsplit",
        kinds: &["text", "bytes"],
        min: 0,
        max: 2,
        positional: 2,
        keywords: &["sep", "maxsplit"],
        result: ResultKind::List,
    },
    Method {
        packed: false,
        descriptor: false,
        name: "splitlines",
        kinds: &["text"],
        min: 0,
        max: 1,
        positional: 1,
        keywords: &["keepends"],
        result: ResultKind::List,
    },
    method("symmetric_difference", 1, 1, ResultKind::Set, &["set"]),
    method(
        "symmetric_difference_update",
        1,
        1,
        ResultKind::None,
        &["set"],
    ),
    packed_method("intersection_update", ResultKind::None, &["set"]),
    packed_method("difference_update", ResultKind::None, &["set"]),
    method("issubset", 1, 1, ResultKind::Bool, &["set"]),
    method("issuperset", 1, 1, ResultKind::Bool, &["set"]),
    method("isdisjoint", 1, 1, ResultKind::Bool, &["set"]),
    Method {
        packed: false,
        descriptor: false,
        name: "decode",
        kinds: &["bytes"],
        min: 0,
        max: 2,
        positional: 2,
        keywords: &["encoding", "errors"],
        result: ResultKind::Str,
    },
    Method {
        packed: false,
        descriptor: false,
        name: "encode",
        kinds: &["text"],
        min: 0,
        max: 2,
        positional: 2,
        keywords: &["encoding", "errors"],
        result: ResultKind::Unknown,
    },
    method("is_integer", 0, 0, ResultKind::Bool, &["float"]),
    method(
        "as_integer_ratio",
        0,
        0,
        ResultKind::Tuple,
        &["float", "integer"],
    ),
    method("bit_length", 0, 0, ResultKind::Int, &["integer"]),
    method("bit_count", 0, 0, ResultKind::Int, &["integer"]),
    Method {
        name: "to_bytes",
        min: 0,
        max: 3,
        positional: 2,
        packed: false,
        descriptor: false,
        kinds: &["integer", "bool"],
        keywords: &["length", "byteorder", "signed"],
        result: ResultKind::Unknown,
    },
    Method {
        name: "from_bytes",
        min: 1,
        max: 3,
        positional: 2,
        packed: false,
        descriptor: false,
        kinds: &["integer", "type:int"],
        keywords: &["bytes", "byteorder", "signed"],
        result: ResultKind::Int,
    },
    method("hex", 0, 0, ResultKind::Str, &["float"]),
    method("isalpha", 0, 0, ResultKind::Bool, &["text"]),
    method("isalnum", 0, 0, ResultKind::Bool, &["text"]),
    method("isdecimal", 0, 0, ResultKind::Bool, &["text"]),
    method("isdigit", 0, 0, ResultKind::Bool, &["text"]),
    method("isnumeric", 0, 0, ResultKind::Bool, &["text"]),
    method("islower", 0, 0, ResultKind::Bool, &["text"]),
    method("isupper", 0, 0, ResultKind::Bool, &["text"]),
    method("istitle", 0, 0, ResultKind::Bool, &["text"]),
    method("isspace", 0, 0, ResultKind::Bool, &["text"]),
    method("isprintable", 0, 0, ResultKind::Bool, &["text"]),
    method("isascii", 0, 0, ResultKind::Bool, &["text"]),
    method("isidentifier", 0, 0, ResultKind::Bool, &["text"]),
    method("cancel", 0, 0, ResultKind::Bool, &["task"]),
];

/// Pure value properties, authored once for both lookup and name projection.
const PROPERTIES: &[(&str, &str, &str)] = &[
    ("integer", "real", "value"),
    ("integer", "imag", "0"),
    ("integer", "numerator", "value"),
    ("integer", "denominator", "1"),
    ("float", "real", "value"),
    ("float", "imag", "0.0"),
    ("tuple", "__name__", "invoke py.name(value)"),
    ("tuple", "start", "invoke py.slice.start(value)"),
    ("tuple", "stop", "invoke py.slice.stop(value)"),
    ("tuple", "step", "invoke py.slice.step(value)"),
    ("error", "args", "invoke py.exc.args(value)"),
    ("error", "__cause__", "invoke py.exc.cause(value)"),
];

/// The names of the methods the dialect has.
pub fn method_names() -> Vec<&'static str> {
    METHODS.iter().map(|method| method.name).collect()
}

/// The runtime attribute table is a projection of the call catalogue. Guest
/// records are reachable only for private installed modules; package values
/// continue through their declared member dispatch.
pub fn attribute_helpers() -> String {
    let owners = method_attributes();
    let mut source = String::from(
        "use py.type.name\nuse py.bind\nuse py.name\nuse py.slice.start\nuse py.slice.stop\nuse py.slice.step\nuse kind\nuse same\nuse bool.not\nuse record.contains\nuse py.no_attribute\nuse py.value_attribute\nuse py.getattr\nuse py.attribute\nuse py.fail\nuse py.exc.args\nuse py.exc.cause\nuse list.len\nuse record.keys\n",
    );
    for method in METHODS {
        source.push_str(&format!("use py.method.{}\n", method.name));
    }
    source.push_str("\nfunction py.value_attribute(value: Any, name: Text) -> Any\nkernel 1\ncharge 32\nbody {\nlet k = kind(value)\n");
    let property_kinds: std::collections::BTreeSet<_> =
        PROPERTIES.iter().map(|(kind, _, _)| *kind).collect();
    for kind in property_kinds {
        source.push_str(&format!("if {} {{\n", kind_condition(kind, "value")));
        for (_, name, expression) in PROPERTIES.iter().filter(|(owner, _, _)| *owner == kind) {
            source.push_str(&format!(
                "if same(name, {name:?}) {{ return {expression} }}\n"
            ));
        }
        source.push_str("}\n");
    }
    source.push_str("return absent\n}\n\nfunction py.attribute_names(value: Any) -> List(Text)\nkernel 1\ncharge sum(128, size(result))\nbody {\nlet k = kind(value)\nlet out = []\nif same(k, \"record\") { if record.contains(value, \"__py_module\") { for name in record.keys(value) { if bool.not(same(name, \"__py_module\")) { set out[list.len(out)] = name } } return out } }\n");
    let mut kinds: std::collections::BTreeSet<_> = owners.keys().map(String::as_str).collect();
    kinds.extend(PROPERTIES.iter().map(|(kind, _, _)| *kind));
    let mut kinds = kinds.into_iter().collect::<Vec<_>>();
    kinds.sort_by_key(|kind| (!kind.starts_with("type:"), *kind));
    for kind in kinds {
        source.push_str(&format!("if {} {{\n", kind_condition(kind, "value")));
        for (method, _) in owners.get(kind).into_iter().flatten() {
            source.push_str(&format!("set out[list.len(out)] = {:?}\n", method.name));
        }
        for (_, name, _) in PROPERTIES.iter().filter(|(owner, _, _)| *owner == kind) {
            source.push_str(&format!("set out[list.len(out)] = {name:?}\n"));
        }
        source.push_str("return out\n}\n");
    }
    source.push_str("return out\n}\n");
    source.push_str("\nfunction py.getattr(receiver: Any, name: Any, default?: Any) -> Any\nkernel 1\ncharge sum(12, size(name))\nbody {\nif bool.not(same(kind(name), \"text\")) { do invoke py.fail(\"TypeError\", \"attribute name must be string\") }\nreturn invoke py.attribute(receiver, name, default)\n}\n");
    source.push_str("\nfunction py.attribute(receiver: Any, name: Text, default: Any) -> Any\nkernel 1\ncharge sum(1024, size(name))\nbody {\nlet k = kind(receiver)\nif bool.not(same(invoke py.type.name(receiver), absent)) { set k = \"type\" }\nif same(k, \"record\") { if record.contains(receiver, \"__py_module\") { if bool.not(same(name, \"__py_module\")) { if record.contains(receiver, name) { return receiver[name] } } } }\nlet property = invoke py.value_attribute(receiver, name)\nif bool.not(same(property, absent)) { return property }\n");
    for (kind, methods) in &owners {
        source.push_str(&format!("if {} {{\n", kind_condition(kind, "receiver")));
        for (method, unbound_kind) in methods {
            let names: Vec<_> = (0..method.max).map(|i| format!("arg{i}")).collect();
            let mut params: Vec<_> = if method.packed {
                vec![("arguments", 3, false), ("keywords", 4, false)]
            } else {
                names
                    .iter()
                    .enumerate()
                    .map(|(index, name)| {
                        let keyword = method
                            .keywords
                            .get(index)
                            .is_some_and(|name| !name.is_empty());
                        let spelling = if keyword {
                            method.keywords[index]
                        } else {
                            name.as_str()
                        };
                        let mode = if index >= method.positional || keyword && method.name == "sort"
                        {
                            2
                        } else if keyword {
                            1
                        } else {
                            0
                        };
                        (spelling, mode, index >= method.min)
                    })
                    .collect()
            };
            if unbound_kind.is_some() {
                params.insert(0, ("self", 0, false));
            }
            let args = (usize::from(unbound_kind.is_some())..params.len())
                .map(|index| format!("argument{index}"))
                .collect::<Vec<_>>()
                .join(", ");
            let comma = if args.is_empty() { "" } else { ", " };
            let receiver = if unbound_kind.is_some() {
                "argument0"
            } else {
                "receiver"
            };
            let check = unbound_kind.map_or_else(String::new, |kind| {
                format!("if bool.not(same(kind(argument0), {kind:?})) {{ do invoke py.fail(\"TypeError\", \"descriptor requires a {kind} receiver\") }}\n")
            });
            let body = format!(
                "{check}let answer = invoke py.method.{}({receiver}{comma}{args}) return answer",
                method.name
            );
            let value = crate::helper_value(method.name, &params, &body);
            source.push_str(&format!(
                "if same(name, {:?}) {{ return {value} }}\n",
                method.name
            ));
        }
        source.push_str("}\n");
    }
    source.push_str("if bool.not(same(default, absent)) { return default }\ndo invoke py.no_attribute(receiver, name)\nreturn null\n}\n\nfunction py.hasattr(receiver: Any, name: Any) -> Bool\nkernel 1\ncharge sum(12, size(name))\nbody {\ntry { let value = invoke py.getattr(receiver, name) return true } catch problem { if same(problem.kind, \"AttributeError\") { return false } throw problem }\n}\n");
    source
}

/// Project instance methods as descriptors on their type, retaining explicitly
/// declared class methods' binding. Lookup and dir consume this same projection.
fn method_attributes()
-> std::collections::BTreeMap<String, Vec<(&'static Method, Option<&'static str>)>> {
    let mut owners = std::collections::BTreeMap::<String, Vec<_>>::new();
    for method in METHODS {
        for kind in method.kinds {
            owners
                .entry((*kind).into())
                .or_default()
                .push((method, None));
            if !method.descriptor || kind.starts_with("type:") {
                continue;
            }
            // Kernel kinds are translated only at the Python type boundary.
            let name = match *kind {
                "text" => "str",
                "integer" => "int",
                "map" => "dict",
                "task" => "Task",
                name => name,
            };
            let class = format!("type:{name}");
            if !method.kinds.contains(&class.as_str()) {
                owners.entry(class).or_default().push((method, Some(*kind)));
            }
        }
    }
    owners
}

/// Catalogue kinds include class-level methods of the guest's type tokens.
fn kind_condition(kind: &str, value: &str) -> String {
    if let Some(name) = kind.strip_prefix("type:") {
        format!("same(invoke py.type.name({value}), {name:?})")
    } else {
        format!("same(k, {kind:?})")
    }
}
