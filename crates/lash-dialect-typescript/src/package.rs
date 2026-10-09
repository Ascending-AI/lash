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
    let mut definitions = define_functions(include_str!("helpers/exotic.kernel"), library)?;
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
    for (name, rows) in &table.properties {
        let source = dispatcher("property", name, rows, "ts.get", "");
        definitions.extend(define_functions(&source, library)?);
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
    for (receiver, function) in rows {
        source.push_str(&format!(
            "  if same(receiver, \"{}\") {{\n    let outcome = invoke {function}({passed})\n    \
             return outcome\n  }}\n",
            receiver.tag()
        ));
    }
    let separator = if fallback_args.is_empty() { "" } else { ", " };
    source.push_str(&format!(
        "  let outcome = invoke {fallback}(this, \"{name}\"{separator}{fallback_args})\n  \
         return outcome\n}}\n"
    ));
    source
}
