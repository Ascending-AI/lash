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
/// reader per member and property name the built-in rows mention.
pub fn define_helpers(library: &mut NamedLibrary) -> Result<Vec<FunctionDefinition>, SourceError> {
    define_helpers_from(library, builtins::objects())
}

/// [`define_helpers`] with the given built-in objects' helper sources.
pub(crate) fn define_helpers_from(
    library: &mut NamedLibrary,
    objects: Vec<builtins::Object>,
) -> Result<Vec<FunctionDefinition>, SourceError> {
    let mut definitions = define_functions(
        include_str!("helpers/number_primitive.kernel"),
        library,
        helper_operations(),
    )?;
    definitions.extend(define_functions(
        include_str!("helpers/regexp_data.kernel"),
        library,
        helper_operations(),
    )?);
    definitions.extend(define_functions(
        include_str!("helpers/exotic.kernel"),
        library,
        helper_operations(),
    )?);
    definitions.extend(define_functions(
        include_str!("helpers/core.kernel"),
        library,
        helper_operations(),
    )?);
    let table = builtins::table();
    for object in objects {
        if object.rows.iter().any(|row| {
            matches!(
                row,
                builtins::Row::Method {
                    path: "Array.prototype.toLocaleString",
                    ..
                }
            )
        }) {
            definitions.extend(define_functions(
                &builtins::locale_element(table),
                library,
                helper_operations(),
            )?);
        }
        definitions.extend(define_functions(
            object.source,
            library,
            helper_operations(),
        )?);
    }
    for source in builtins::value_sources(table) {
        definitions.extend(define_functions(&source, library, helper_operations())?);
    }
    for name in &table.member_names {
        definitions.extend(define_functions(
            &builtins::member_reader(table, name),
            library,
            helper_operations(),
        )?);
    }
    for (name, rows) in &table.properties {
        let source = dispatcher(table, name, rows);
        definitions.extend(define_functions(&source, library, helper_operations())?);
    }
    definitions.extend(define_functions(
        &builtin_name(table),
        library,
        helper_operations(),
    )?);
    definitions.extend(define_functions(
        &computed_reader(table, false),
        library,
        helper_operations(),
    )?);
    definitions.extend(define_functions(
        &computed_reader(table, true),
        library,
        helper_operations(),
    )?);
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

/// The kernel text of a property reader: the receiver kind chooses its row,
/// falling back to an own member when no row answers.
pub(crate) fn dispatcher(
    table: &builtins::Table,
    name: &str,
    rows: &[(Receiver, &'static str)],
) -> String {
    let (reader, read) = reader(table, name);
    let mut source = String::from("use same\nuse kind\nuse text.concat\nuse ts.receiver\n");
    let record_rows = rows
        .iter()
        .any(|(receiver, _)| *receiver == Receiver::Record);
    if record_rows {
        source.push_str("use record.contains\n");
    }
    source.push_str(&format!("use {reader}\n"));
    let mut named: Vec<&str> = rows.iter().map(|(_, function)| *function).collect();
    named.sort_unstable();
    named.dedup();
    for function in named {
        source.push_str(&format!("use {function}\n"));
    }
    let params = "this: Any";
    let passed = "this";
    let member = format!("let outcome = {read}");
    source.push_str(&format!(
        "function ts.property.{name}({params}) -> Any\nkernel 1\n\
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
        let passed = "items";
        source.push_str(&format!(
            "  if same(receiver, \"brand:typescript.regex_match\") {{\n    let items = this.items\n    let outcome = invoke {function}({passed})\n    return outcome\n  }}\n"
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
    set receiver = invoke ts.receiver(this)
  } else {
    if same(receiver, \"list\") {} else {
      if same(receiver, \"text\") {} else { set receiver = invoke ts.receiver(this) }
    }
  }
";

/// The names a computed read finds among the rows: each property row's,
/// then each member reader's, once each, in `text.compare`'s order (code
/// point order, as `str` orders them).
fn row_names(table: &'static builtins::Table) -> Vec<(&'static str, String)> {
    // A property row takes precedence over the member reader, as for
    // a literal name.
    let mut rows: Vec<(&str, String)> = Vec::new();
    rows.extend(
        table
            .properties
            .keys()
            .map(|name| (*name, format!("ts.property.{name}"))),
    );
    rows.extend(
        table
            .member_names
            .iter()
            .map(|name| (*name, format!("ts.member.{name}"))),
    );
    let mut seen = std::collections::BTreeSet::new();
    rows.retain(|(name, _)| seen.insert(*name));
    rows.sort_by(|left, right| left.0.cmp(right.0));
    rows
}

/// Every name a computed read finds among the built-in rows, in code point
/// order: the table `ts.builtin_name` searches, which its native searches
/// too.
pub fn builtin_names() -> Vec<&'static str> {
    row_names(builtins::table())
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// `ts.builtin_name(name)`: whether a text is the name of a row a computed
/// read finds, by halving the fixed table of row names. A plain record's
/// read by any other text is its own field or `undefined` (`ts.read`).
fn builtin_name(table: &'static builtins::Table) -> String {
    let branches: Vec<(&str, String)> = row_names(table)
        .into_iter()
        .map(|(name, _)| (name, "let outcome = true".to_string()))
        .collect();
    let mut source = String::from(
        "use same\nuse num.lt\nuse text.compare\n\
         function ts.builtin_name(name: Any) -> Bool\nkernel 1\ncharge 4\nnative\nbody {\n",
    );
    search(&branches, 1, &mut source);
    source.push_str("  return false\n}\n");
    source
}

/// Computed names use the same rows and readers as literal names. The key
/// is coerced once, preserving its observable conversion. The name is
/// found among the rows by halving them in name order with
/// `text.compare`, so a read costs the logarithm of the rows, not their
/// count. Numeric keys use only rows whose names may be number
/// spellings; the Number parameter refuses a mistyped key before dispatch.
fn computed_reader(table: &'static builtins::Table, numeric: bool) -> String {
    let fallback = "ts.read";
    let mut source = format!(
        "use bool.not\nuse same\nuse kind\nuse num.lt\nuse text.compare\nuse record.contains\nuse ts.builtin_name\nuse ts.to_property_key\nuse ts.require_object_coercible\nuse {fallback}\n"
    );
    source.push_str("use num.le\nuse num.to_float\nuse list.len\nuse text.utf16_len\nuse ts.number_index\nuse ts.get\n");
    // Calling a member reads its value before applying it.
    let mut rows = row_names(table);
    if numeric {
        // Use the same key domain as plain-record alias analysis. It includes
        // exact kernel integers, NaN and infinities, and may retain extra rows.
        rows.retain(|(name, _)| crate::types::numeric_name(name));
    }
    for (_, function) in &rows {
        source.push_str(&format!("use {function}\n"));
    }
    let branches: Vec<(&str, String)> = rows
        .iter()
        .map(|(name, function)| {
            let outcome = format!("let outcome = invoke {function}(this)");
            (*name, outcome)
        })
        .collect();
    let (function, params) = if numeric {
        ("ts.get_numeric", "this: Any, key: Number")
    } else {
        ("ts.get_computed", "this: Any, key: Any")
    };
    let own = "return own";
    source.push_str(&format!(
        "function {function}({params}) -> Any\nkernel 1\ncharge 4\nbody {{\n"
    ));
    // A plain object's own field is what every row and reader gives for
    // its name, so a text key naming one needs neither coercion nor rows.
    // A key that is no text raises `type_error` reading the field, and an
    // absent field may be an inherited name: both take the rows, unless
    // the key is a text no row has, which `ts.read` answers with the
    // absent field. A record with no marker field is plain without
    // building the marker to compare.
    source.push_str(&format!(
        "  if same(kind(this), \"record\") {{
    if bool.not(record.contains(this, \"__lash_brand\")) {{
      let own = absent
      try {{ set own = this[key] }} catch generic {{}}
      if bool.not(same(own, absent)) {{
      {own}
      }}
      if same(kind(key), \"text\") {{
        if ts.builtin_name(key) {{}} else {{ return absent }}
      }}
    }}
  }}
  do invoke ts.require_object_coercible(this)\n"
    ));
    // A number naming an element of a list or a text spells a row's name
    // only if some row's name is all digits; then every key takes the rows.
    if !rows
        .iter()
        .any(|(name, _)| name.bytes().all(|byte| byte.is_ascii_digit()))
    {
        source.push_str(ELEMENT_READ);
    }
    source.push_str("  let name = invoke ts.to_property_key(key)\n");
    search(&branches, 1, &mut source);
    let args = "this, name";
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

/// Typed service declarations shared by helper loading and runtime publication.
/// Slots are local to documents assembled by this dialect; authored imports
/// reserve them before allocation.
pub fn helper_operations() -> &'static [lash_kernel_dialect::authoring::HelperOperation] {
    &lash_dialect_services::TYPESCRIPT
}
