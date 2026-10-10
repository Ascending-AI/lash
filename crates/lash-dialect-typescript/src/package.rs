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
    let mut source = String::from("use same\nuse ts.receiver\n");
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
         errors \"type_error\"\ncharge {}\nbody {{\n  let receiver = invoke ts.receiver(this)\n",
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

/// Computed names use the same rows and readers as literal names. The key
/// is coerced once, preserving its observable conversion.
fn computed_dispatcher(table: &builtins::Table, call: bool) -> String {
    let family = if call {
        "call_computed"
    } else {
        "get_computed"
    };
    let fallback = if call { "ts.call_member" } else { "ts.read" };
    let mut source = format!(
        "use same\nuse ts.to_property_key\nuse ts.require_object_coercible\nuse {fallback}\n"
    );
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
    let params = if call {
        "this: Any, key: Any, args: List(Any)"
    } else {
        "this: Any, key: Any"
    };
    source.push_str(&format!("function ts.{family}({params}) -> Any\nkernel 1\ncharge {}\nbody {{\n  do invoke ts.require_object_coercible(this)\n  let name = invoke ts.to_property_key(key)\n", 4 + rows.len()));
    for (name, function, read_then_call) in &rows {
        let outcome = if *read_then_call {
            format!(
                "let member = invoke {function}(this)\n    let outcome = invoke ts.call_value(member, this, name, args)"
            )
        } else if call {
            format!("let outcome = invoke {function}(this, args)")
        } else {
            format!("let outcome = invoke {function}(this)")
        };
        source.push_str(&format!(
            "  if same(name, \"{name}\") {{\n    {outcome}\n    return outcome\n  }}\n"
        ));
    }
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
