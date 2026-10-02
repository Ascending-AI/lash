//! The IR specification names every name the IR surface invents.
//!
//! `crates/lashlang/docs/ir-specification.md` is the dialect-independent
//! specification of the IR a module artifact stores and the VM runs. This module reads the
//! enum and registry definitions out of the crate's own sources and refuses
//! every variant, builtin, intrinsic, heap kind and version spelling the
//! document does not carry, so adding to the IR without specifying it fails
//! here. The document's coverage is named by backticked spelling: each
//! surface name must appear as `` `Name` ``.

const AST: &str = include_str!("../src/ast.rs");
const AST_NUMBER: &str = include_str!("../src/ast_number.rs");
const AST_ROLES: &str = include_str!("../src/ast_roles.rs");
const BUILTINS: &str = include_str!("../src/builtins.rs");
const VALUE: &str = include_str!("../src/runtime/value.rs");
const HEAP_OBJECT: &str = include_str!("../src/runtime/heap/object.rs");
const ARTIFACT: &str = include_str!("../src/artifact.rs");
const ARTIFACT_IDENTITY: &str = include_str!("../src/artifact_identity.rs");
const ARTIFACT_HASH_WRITER: &str = include_str!("../src/artifact_hash_writer.rs");
const SPEC: &str = include_str!("../docs/ir-specification.md");

/// The slice after `enum <name> {`. These enums always open their brace on
/// the declaration line.
fn enum_body<'a>(source: &'a str, name: &str) -> &'a str {
    let marker = format!("enum {name} {{");
    let start = source
        .find(&marker)
        .unwrap_or_else(|| panic!("no `{marker}` in the scanned source"));
    &source[start + marker.len()..]
}

/// The variant names of `enum <name>`: the lines at the enum's own depth
/// whose first token is an identifier. Comment and attribute lines carry
/// none, and a variant's payload sits one depth deeper.
fn enum_variants(source: &str, name: &str) -> Vec<String> {
    let mut variants = Vec::new();
    let mut depth = 0usize;
    for raw in enum_body(source, name).lines() {
        let line = raw.split("//").next().unwrap_or_default().trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if depth == 0 {
            if line.starts_with('}') {
                break;
            }
            let variant: String = line
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect();
            if variant.chars().next().is_some_and(char::is_uppercase) {
                variants.push(variant);
            }
        }
        depth += line.matches('{').count();
        depth = depth.saturating_sub(line.matches('}').count());
    }
    assert!(
        !variants.is_empty(),
        "no variants found for `enum {name}`; the sweep is broken"
    );
    variants
}

/// The string literals of `const <name>: <ty> = [ ... ];`, one per line.
fn const_string_list(source: &str, name: &str) -> Vec<String> {
    let start = source
        .find(name)
        .unwrap_or_else(|| panic!("no `{name}` in the scanned source"));
    let body = &source[start..];
    let body = &body[body
        .find("= [")
        .unwrap_or_else(|| panic!("string list opens with `= [`"))..];
    let end = body
        .find(']')
        .unwrap_or_else(|| panic!("string list has a closing bracket"));
    let values: Vec<String> = body[..end]
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            line.strip_prefix('"')
                .and_then(|rest| rest.strip_suffix("\","))
                .map(str::to_string)
        })
        .collect();
    assert!(
        !values.is_empty(),
        "no entries found for `{name}`; the sweep is broken"
    );
    values
}

/// The `name: "..."` entries of `const <name>: &[Builtin] = &[ ... ];`.
fn builtin_names(source: &str, registry: &str) -> Vec<String> {
    let start = source
        .find(registry)
        .unwrap_or_else(|| panic!("no `{registry}` in the scanned source"));
    let body = &source[start..];
    let end = body
        .find("];")
        .unwrap_or_else(|| panic!("builtin registry has a closing bracket"));
    let values: Vec<String> = body[..end]
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            line.strip_prefix("name: \"")
                .and_then(|rest| rest.strip_suffix("\","))
                .map(str::to_string)
        })
        .collect();
    assert!(
        !values.is_empty(),
        "no entries found for `{registry}`; the sweep is broken"
    );
    values
}

/// Every `const NAME: <ty> = <literal>;` pair in `source` — the durable
/// version and domain spellings the document must carry.
fn constants(source: &str, ty: &str) -> Vec<(String, String)> {
    let marker = format!(": {ty} = ");
    source
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line
                .strip_prefix("pub(crate) ")
                .or_else(|| line.strip_prefix("pub(super) "))
                .or_else(|| line.strip_prefix("pub "))
                .unwrap_or(line);
            let rest = line.strip_prefix("const ")?;
            let (name, rest) = rest.split_once(marker.as_str())?;
            let value = if ty == "&str" {
                // A string literal only — `env!`-valued constants carry no
                // spelling to pin.
                rest.strip_prefix('"')?.strip_suffix("\";")?.to_string()
            } else {
                rest.strip_suffix(';')?.to_string()
            };
            Some((name.to_string(), value))
        })
        .collect()
}

#[test]
fn every_ir_variant_is_specified() {
    let mut missing = Vec::new();
    for (origin, source, enums) in [
        (
            "ast.rs",
            AST,
            [
                "AstRoot",
                "Declaration",
                "AssignPathStep",
                "Expr",
                "MethodKey",
                "TypeExpr",
                "CoercingUnaryOp",
                "CoercingBinaryOp",
                "OperandLogicalOp",
                "ProcessTypeKind",
                "ProcessTypeWire",
            ]
            .as_slice(),
        ),
        (
            "ast_number.rs",
            AST_NUMBER,
            ["NonFiniteNumber", "IrNumber"].as_slice(),
        ),
        (
            "ast_roles.rs",
            AST_ROLES,
            [
                "ProcessOrigin",
                "StructuralRole",
                "UpdateOperator",
                "BindingVisibility",
            ]
            .as_slice(),
        ),
        ("value.rs", VALUE, ["Value"].as_slice()),
        ("heap/object.rs", HEAP_OBJECT, ["HeapObject"].as_slice()),
    ] {
        for name in enums {
            for variant in enum_variants(source, name) {
                if !SPEC.contains(&format!("`{variant}`")) {
                    missing.push(format!("{origin} `enum {name}` variant `{variant}`"));
                }
            }
        }
    }
    assert!(
        missing.is_empty(),
        "crates/lashlang/docs/ir-specification.md does not specify these IR variants:\n{}",
        missing.join("\n")
    );
}

#[test]
fn every_builtin_and_intrinsic_is_specified() {
    let mut missing = Vec::new();
    for (registry, origin) in [
        ("SOURCE_BUILTINS", "source builtin"),
        ("IR_INTRINSICS", "IR intrinsic"),
    ] {
        for name in builtin_names(BUILTINS, registry) {
            if !SPEC.contains(&format!("`{name}`")) {
                missing.push(format!("{origin} `{name}`"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "crates/lashlang/docs/ir-specification.md does not specify these builtins:\n{}",
        missing.join("\n")
    );
}

#[test]
fn every_heap_kind_is_specified() {
    let mut missing = Vec::new();
    for kind in const_string_list(HEAP_OBJECT, "HEAP_OBJECT_KINDS") {
        if !SPEC.contains(&format!("`{kind}`")) {
            missing.push(format!("heap kind `{kind}`"));
        }
    }
    assert!(
        missing.is_empty(),
        "crates/lashlang/docs/ir-specification.md does not specify these heap kinds:\n{}",
        missing.join("\n")
    );
}

#[test]
fn every_durable_version_spelling_is_specified() {
    let mut missing = Vec::new();
    for (origin, source) in [
        ("artifact.rs", ARTIFACT),
        ("artifact_identity.rs", ARTIFACT_IDENTITY),
        ("artifact_hash_writer.rs", ARTIFACT_HASH_WRITER),
        ("ast_roles.rs", AST_ROLES),
    ] {
        for (name, value) in constants(source, "&str") {
            if !SPEC.contains(name.as_str()) {
                missing.push(format!("{origin} constant `{name}`"));
            }
            if !SPEC.contains(&format!("`{value}`")) {
                missing.push(format!("{origin} `{name}` value `{value}`"));
            }
        }
    }
    for (name, value) in constants(ARTIFACT, "u32") {
        if !SPEC.contains(name.as_str()) {
            missing.push(format!("artifact.rs constant `{name}`"));
        }
        if !SPEC.contains(&format!("`{value}`")) {
            missing.push(format!("artifact.rs `{name}` encoding `{value}`"));
        }
    }
    for (name, value) in constants(AST, "usize") {
        if !SPEC.contains(name.as_str()) {
            missing.push(format!("ast.rs constant `{name}`"));
        }
        if !SPEC.contains(&format!("`{value}`")) {
            missing.push(format!("ast.rs `{name}` limit `{value}`"));
        }
    }
    // The semantic-hash version is claimed by the manifest row; the spec
    // carries its current spelling.
    if !SPEC.contains(lashlang::LASHLANG_SEMANTIC_HASH_VERSION) {
        missing.push(format!(
            "semantic hash version `{}`",
            lashlang::LASHLANG_SEMANTIC_HASH_VERSION
        ));
    }
    assert!(
        missing.is_empty(),
        "crates/lashlang/docs/ir-specification.md does not specify these durable spellings:\n{}",
        missing.join("\n")
    );
}
