//! The functions the TypeScript dialect ships.

use lash_kernel_dialect::{NamedLibrary, SourceError, define_functions};
use lash_kernel_doc::FunctionDefinition;

use crate::builtins::{self, Receiver};

/// Defines the dialect's helpers against `library`, which holds the kernel
/// library and any extension the built-ins call, adds them to it, and
/// returns them in the order an embedder registers them.
///
/// The core operations come first, then each built-in object's helpers,
/// then one generated dispatcher per method and property name the built-in
/// rows mention.
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
    for (name, rows) in &table.methods {
        let source = dispatcher("method", name, rows, "ts.call_member", "args");
        definitions.extend(define_functions(&source, library)?);
    }
    for (name, rows) in &table.methods {
        definitions.extend(define_functions(&method_reader(name, rows), library)?);
    }
    for (name, rows) in &table.properties {
        let source = dispatcher("property", name, rows, "ts.get", "");
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

/// The kernel text of `ts.<family>.<name>`: the receiver's kind chooses the
/// row, and a receiver no row names falls back to its own property.
pub(crate) fn dispatcher(
    family: &str,
    name: &str,
    rows: &[(Receiver, &'static str)],
    fallback: &str,
    fallback_args: &str,
) -> String {
    let mut source = String::from("use same\nuse ts.receiver\n");
    let record_rows = rows
        .iter()
        .any(|(receiver, _)| *receiver == Receiver::Record);
    if record_rows {
        source.push_str("use record.contains\n");
    }
    source.push_str(&format!("use {fallback}\n"));
    let mut named: Vec<&str> = rows.iter().map(|(_, function)| *function).collect();
    named.sort_unstable();
    named.dedup();
    for function in named {
        source.push_str(&format!("use {function}\n"));
    }
    // A method's rows take the receiver and the argument list; a property's
    // rows take the receiver alone.
    let (params, passed) = if fallback_args.is_empty() {
        ("this: Any", "this")
    } else {
        ("this: Any, args: List(Any)", "this, args")
    };
    source.push_str(&format!(
        "function ts.{family}.{name}({params}) -> Any\nkernel 1\n\
         errors \"type_error\"\ncharge {}\nbody {{\n  let receiver = invoke ts.receiver(this)\n",
        4 + rows.len()
    ));
    if record_rows {
        let separator = if fallback_args.is_empty() { "" } else { ", " };
        source.push_str(&format!("  if same(receiver, \"record\") {{\n    if record.contains(this, \"{name}\") {{\n      let outcome = invoke {fallback}(this, \"{name}\"{separator}{fallback_args})\n      return outcome\n    }}\n  }}\n"));
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
        let passed = if fallback_args.is_empty() {
            "items"
        } else {
            "items, args"
        };
        source.push_str(&format!(
            "  if same(receiver, \"brand:regex.match\") {{\n    let items = this.items\n    let outcome = invoke {function}({passed})\n    return outcome\n  }}\n"
        ));
    }
    let separator = if fallback_args.is_empty() { "" } else { ", " };
    source.push_str(&format!(
        "  let outcome = invoke {fallback}(this, \"{name}\"{separator}{fallback_args})\n  \
         return outcome\n}}\n"
    ));
    source
}

/// Reading an existing method gives an unbound callable. An own callable
/// field on a record is resolved through ts.get instead.
fn method_reader(name: &str, rows: &[(Receiver, &'static str)]) -> String {
    let mut source = format!("use same\nuse ts.receiver\nuse ts.get\nuse ts.method.{name}\n");
    let record_rows = rows
        .iter()
        .any(|(receiver, _)| *receiver == Receiver::Record);
    if record_rows {
        source.push_str("use record.contains\n");
    }
    source.push_str(&format!(
        "function ts.member.{name}(this: Any) -> Any\nkernel 1\ncharge {}\nbody {{\n  let receiver = invoke ts.receiver(this)\n",
        4 + rows.len()
    ));
    if record_rows {
        source.push_str(&format!("  if same(receiver, \"record\") {{\n    if record.contains(this, \"{name}\") {{\n      let outcome = invoke ts.get(this, \"{name}\")\n      return outcome\n    }}\n  }}\n"));
    }
    for (receiver, _) in rows {
        source.push_str(&format!(
            "  if same(receiver, \"{}\") {{\n    return fn(this, args) {{\n      let outcome = invoke ts.method.{name}(this, args)\n      return outcome\n    }}\n  }}\n",
            receiver.tag()
        ));
    }
    if rows.iter().any(|(receiver, _)| *receiver == Receiver::List) {
        source.push_str(&format!(
            "  if same(receiver, \"brand:regex.match\") {{\n    return fn(this, args) {{\n      let outcome = invoke ts.method.{name}(this, args)\n      return outcome\n    }}\n  }}\n"
        ));
    }
    source.push_str(&format!(
        "  let outcome = invoke ts.get(this, \"{name}\")\n  return outcome\n}}\n"
    ));
    source
}

/// Computed names use the same rows as literal names. The key is coerced
/// once, preserving its observable conversion.
fn computed_dispatcher(table: &builtins::Table, call: bool) -> String {
    let family = if call {
        "call_computed"
    } else {
        "get_computed"
    };
    let fallback = if call { "ts.call_member" } else { "ts.get" };
    let mut source = format!(
        "use same\nuse ts.to_property_key\nuse ts.require_object_coercible\nuse {fallback}\n"
    );
    let mut rows = Vec::new();
    if !call {
        rows.extend(
            table
                .properties
                .keys()
                .map(|name| (*name, format!("ts.property.{name}"))),
        );
    }
    rows.extend(table.methods.keys().map(|name| {
        (
            *name,
            format!("ts.{}.{name}", if call { "method" } else { "member" }),
        )
    }));
    // A name may carry both a property row and a method row. Property reads
    // take the property row, as get_member does for a literal name.
    let mut seen = std::collections::BTreeSet::new();
    rows.retain(|(name, _)| seen.insert(*name));
    for (_, function) in &rows {
        source.push_str(&format!("use {function}\n"));
    }
    let params = if call {
        "this: Any, key: Any, args: List(Any)"
    } else {
        "this: Any, key: Any"
    };
    let args = if call { "this, args" } else { "this" };
    source.push_str(&format!("function ts.{family}({params}) -> Any\nkernel 1\ncharge {}\nbody {{\n  do invoke ts.require_object_coercible(this)\n  let name = invoke ts.to_property_key(key)\n", 4 + rows.len()));
    for (name, function) in &rows {
        source.push_str(&format!("  if same(name, \"{name}\") {{\n    let outcome = invoke {function}({args})\n    return outcome\n  }}\n"));
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
