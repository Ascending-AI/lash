//! The functions the TypeScript dialect ships.

use lash_kernel_dialect::{NamedLibrary, SourceError, define_functions};
use lash_kernel_doc::FunctionDefinition;

use crate::builtins::{self, Receiver};

/// Defines the dialect's helpers against `library`, which holds the kernel
/// library and any extension the built-ins call, adds them to it, and
/// returns them in the order an embedder registers them.
///
/// The core operations come first, then each built-in object's helpers,
/// then the functions that make built-ins values, then one generated
/// dispatcher per method and property name the built-in rows mention.
pub fn define_helpers(library: &mut NamedLibrary) -> Result<Vec<FunctionDefinition>, SourceError> {
    let mut definitions =
        define_functions(include_str!("helpers/number_primitive.kernel"), library)?;
    definitions.extend(define_functions(
        include_str!("helpers/regexp_data.kernel"),
        library,
    )?);
    definitions.extend(define_functions(
        include_str!("helpers/exotic.kernel"),
        library,
    )?);
    definitions.extend(define_functions(
        include_str!("helpers/core.kernel"),
        library,
    )?);
    for object in builtins::objects() {
        definitions.extend(define_functions(object.source, library)?);
    }
    let table = builtins::table();
    for source in builtins::value_sources(table) {
        definitions.extend(define_functions(&source, library)?);
    }
    for name in &table.member_names {
        definitions.extend(define_functions(
            &builtins::member_reader(table, name),
            library,
        )?);
    }
    for (name, rows) in &table.methods {
        let source = dispatcher(table, "method", name, rows);
        definitions.extend(define_functions(&source, library)?);
    }
    for (name, rows) in &table.properties {
        let source = dispatcher(table, "property", name, rows);
        definitions.extend(define_functions(&source, library)?);
    }
    for call in [false, true] {
        definitions.extend(define_functions(
            &computed_dispatcher(table, call),
            library,
        )?);
    }
    Ok(definitions)
}

/// The function that reads the member `name` where no row answers, and the
/// kernel text of that read of `this`.
fn reader(table: &builtins::Table, name: &str) -> (String, String) {
    if table.member_names.contains(name) {
        let reader = format!("ts.member.{name}");
        let read = format!("invoke {reader}(this)");
        (reader, read)
    } else {
        (
            "ts.read".to_string(),
            format!("invoke ts.read(this, \"{name}\")"),
        )
    }
}

/// The kernel text of `ts.<family>.<name>`: the receiver's kind chooses the
/// row, and a receiver no row names, or a record with a field of that name,
/// gives its member by that name, which a method then calls.
pub(crate) fn dispatcher(
    table: &builtins::Table,
    family: &str,
    name: &str,
    rows: &[(Receiver, &'static str)],
) -> String {
    let method = family == "method";
    let (reader, read) = reader(table, name);
    let mut source = String::from("use same\nuse kind\nuse text.concat\nuse ts.receiver\n");
    let record_rows = rows
        .iter()
        .any(|(receiver, _)| *receiver == Receiver::Record);
    if record_rows {
        source.push_str("use record.contains\n");
    }
    source.push_str(&format!("use {reader}\n"));
    if method {
        source.push_str("use ts.call_value\n");
    }
    let mut named: Vec<&str> = rows.iter().map(|(_, function)| *function).collect();
    named.sort_unstable();
    named.dedup();
    for function in named {
        source.push_str(&format!("use {function}\n"));
    }
    // A method's rows take the receiver and the argument list; a property's
    // rows take the receiver alone.
    let (params, passed) = if method {
        ("this: Any, args: List(Any)", "this, args")
    } else {
        ("this: Any", "this")
    };
    let member = if method {
        format!(
            "let member = {read}\n    let outcome = invoke ts.call_value(member, this, \"{name}\", args)"
        )
    } else {
        format!("let outcome = {read}")
    };
    source.push_str(&format!(
        "function ts.{family}.{name}({params}) -> Any\nkernel 1\n\
         errors \"type_error\"\ncharge {}\nbody {{\n{RECEIVER}",
        4 + rows.len()
    ));
    if record_rows {
        source.push_str(&format!("  if same(receiver, \"record\") {{\n    if record.contains(this, \"{name}\") {{\n      {}\n      return outcome\n    }}\n  }}\n", member.replace("\n    ", "\n      ")));
    }
    for (receiver, function) in rows {
        source.push_str(&format!(
            "  if same(receiver, \"{}\") {{\n    let outcome = invoke {function}({passed})\n    \
             return outcome\n  }}\n",
            receiver.tag()
        ));
    }
    if let Some((_, function)) = rows
        .iter()
        .find(|(receiver, _)| *receiver == Receiver::List)
    {
        let passed = if method { "items, args" } else { "items" };
        source.push_str(&format!(
            "  if same(receiver, \"brand:regex.match\") {{\n    let items = this.items\n    let outcome = invoke {function}({passed})\n    return outcome\n  }}\n"
        ));
    }
    source.push_str(&format!(
        "  {}\n  return outcome\n}}\n",
        member.replace("\n    ", "\n  ")
    ));
    source
}

/// `let receiver = invoke ts.receiver(this)`, with a plain or branded
/// object, an array and a text told apart in place: the receivers most
/// member reads and calls have.
const RECEIVER: &str = "  let receiver = kind(this)
  if same(receiver, \"record\") {
    let brand = this.brand
    if same(brand, absent) {} else {
      if same(kind(brand), \"text\") { set receiver = text.concat(\"brand:\", brand) }
    }
  } else {
    if same(receiver, \"list\") {} else {
      if same(receiver, \"text\") {} else { set receiver = invoke ts.receiver(this) }
    }
  }
";

/// Computed names use the same rows and readers as literal names. The key
/// is coerced once, preserving its observable conversion. The name is
/// found among the rows by halving them in name order with
/// `text.compare`, so a read costs the logarithm of the rows, not their
/// count.
fn computed_dispatcher(table: &builtins::Table, call: bool) -> String {
    let family = if call {
        "call_computed"
    } else {
        "get_computed"
    };
    let fallback = if call { "ts.call_member" } else { "ts.read" };
    let mut source = format!(
        "use same\nuse kind\nuse num.lt\nuse text.compare\nuse ts.to_property_key\nuse ts.require_object_coercible\nuse {fallback}\n"
    );
    if !call {
        source.push_str("use num.le\nuse num.to_float\nuse list.len\nuse text.utf16_len\nuse ts.number_index\nuse ts.get\n");
    }
    if call {
        source.push_str("use ts.call_value\n");
    }
    // A name may carry a property row, a method row and a reader. A read
    // takes the property row, as get_member does for a literal name; a
    // call takes the method row.
    let mut rows: Vec<(&str, String, bool)> = Vec::new();
    if call {
        rows.extend(
            table
                .methods
                .keys()
                .map(|name| (*name, format!("ts.method.{name}"), false)),
        );
    } else {
        rows.extend(
            table
                .properties
                .keys()
                .map(|name| (*name, format!("ts.property.{name}"), false)),
        );
    }
    rows.extend(
        table
            .member_names
            .iter()
            .map(|name| (name.as_str(), format!("ts.member.{name}"), call)),
    );
    let mut seen = std::collections::BTreeSet::new();
    rows.retain(|(name, _, _)| seen.insert(*name));
    for (_, function, _) in &rows {
        source.push_str(&format!("use {function}\n"));
    }
    // `text.compare` orders texts by code point, as `str` orders them.
    rows.sort_by(|left, right| left.0.cmp(right.0));
    let branches: Vec<(&str, String)> = rows
        .iter()
        .map(|(name, function, read_then_call)| {
            let outcome = if *read_then_call {
                format!(
                    "let member = invoke {function}(this)\nlet outcome = invoke ts.call_value(member, this, name, args)"
                )
            } else if call {
                format!("let outcome = invoke {function}(this, args)")
            } else {
                format!("let outcome = invoke {function}(this)")
            };
            (*name, outcome)
        })
        .collect();
    let params = if call {
        "this: Any, key: Any, args: List(Any)"
    } else {
        "this: Any, key: Any"
    };
    let own = if call {
        "let outcome = invoke ts.call_value(own, this, key, args)\n    return outcome"
    } else {
        "return own"
    };
    source.push_str(&format!(
        "function ts.{family}({params}) -> Any\nkernel 1\ncharge 4\nbody {{\n"
    ));
    // A plain object's own field is what every row and reader gives for
    // its name, so a text key naming one needs neither coercion nor rows.
    // A key that is no text raises `type_error` reading the field, and an
    // absent field may be an inherited name: both take the rows.
    source.push_str(&format!(
        "  if same(kind(this), \"record\") {{
    let own = absent
    try {{
      if same(this.brand, absent) {{ set own = this[key] }}
    }} catch generic {{}}
    if same(own, absent) {{}} else {{
    {own}
    }}
  }}
  do invoke ts.require_object_coercible(this)\n"
    ));
    // A number naming an element of a list or a text spells a row's name
    // only if some row's name is all digits; then every key takes the rows.
    if !call
        && !rows
            .iter()
            .any(|(name, _, _)| name.bytes().all(|byte| byte.is_ascii_digit()))
    {
        source.push_str(ELEMENT_READ);
    }
    source.push_str("  let name = invoke ts.to_property_key(key)\n");
    search(&branches, 1, &mut source);
    let args = if call {
        "this, name, args"
    } else {
        "this, name"
    };
    source.push_str(&format!(
        "  let outcome = invoke {fallback}({args})\n  return outcome\n}}\n"
    ));
    source
}

/// A read whose key is a number naming an element of a list or a text: what
/// `ts.read` gives for the key's spelling, without spelling it.
const ELEMENT_READ: &str = "  if same(kind(key), \"float\") {
    let size = -1.0
    if same(kind(this), \"list\") { set size = num.to_float(list.len(this)) }
    if same(kind(this), \"text\") { set size = num.to_float(text.utf16_len(this)) }
    let position = invoke ts.number_index(key, size)
    if num.le(0, position) {
      if same(kind(this), \"list\") {
        let element = this[position]
        if same(element, ()) { return absent }
        return element
      }
      let element = invoke ts.get(this, key)
      return element
    }
  }
";

/// The kernel text that finds `name` among `branches`, sorted by name, and
/// returns what the matching branch gives; no match falls through.
fn search(branches: &[(&str, String)], depth: usize, source: &mut String) {
    let indent = "  ".repeat(depth);
    if branches.len() <= 4 {
        for (name, outcome) in branches {
            let outcome = outcome.replace('\n', &format!("\n{indent}  "));
            source.push_str(&format!(
                "{indent}if same(name, \"{name}\") {{\n{indent}  {outcome}\n{indent}  return outcome\n{indent}}}\n"
            ));
        }
        return;
    }
    let (below, rest) = branches.split_at(branches.len() / 2);
    source.push_str(&format!(
        "{indent}if num.lt(text.compare(name, \"{}\"), 0) {{\n",
        rest[0].0
    ));
    search(below, depth + 1, source);
    source.push_str(&format!("{indent}}} else {{\n"));
    search(rest, depth + 1, source);
    source.push_str(&format!("{indent}}}\n"));
}
