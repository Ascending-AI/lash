//! Closed helper sets generated from the definitions, without reordering bodies.

use lash_kernel_doc::FunctionRegistry;
use std::collections::BTreeSet;

pub fn enum_source<'a>(
    ty: &str,
    names: impl IntoIterator<Item = &'a str>,
) -> Result<String, String> {
    let mut unique = BTreeSet::new();
    let mut variants = BTreeSet::new();
    let mut rows = Vec::new();
    for name in names {
        if !unique.insert(name) {
            return Err(format!("duplicate helper `{name}`"));
        }
        // Preserve case and escape separators: parseInt and parse_int are distinct helpers.
        let variant = format!("H_{}", name.replace('_', "_u").replace('.', "_d"));
        if !variants.insert(variant.clone()) {
            return Err(format!("helper variant collision for `{name}`"));
        }
        rows.push((name, variant));
    }
    rows.sort_unstable();
    let mut source = format!(
        "// Generated from the registered definitions; review changes to this set.\n#[allow(non_camel_case_types)]\n#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]\npub enum {ty} {{\n"
    );
    for (_, variant) in &rows {
        source.push_str(&format!("    {variant},\n"));
    }
    source.push_str(&format!(
        "}}\nimpl {ty} {{\n    pub const ALL: &'static [Self] = &[\n"
    ));
    for (_, variant) in &rows {
        source.push_str(&format!("        Self::{variant},\n"));
    }
    source.push_str("    ];\n    pub fn name(self) -> &'static str {\n        match self {\n");
    for (name, variant) in &rows {
        source.push_str(&format!("            Self::{variant} => {name:?},\n"));
    }
    source.push_str("        }\n    }\n    pub fn from_name(name: &str) -> Option<Self> {\n        Self::ALL.iter().copied().find(|helper| helper.name() == name)\n    }\n}\n");
    Ok(source)
}

pub fn kernel_names(registry: &FunctionRegistry) -> Result<Vec<&str>, String> {
    let names: Vec<_> = registry
        .iter()
        .map(|(_, f)| f.definition().name.as_str())
        .collect();
    for name in &names {
        let family = name.split_once('.').map_or("", |(family, _)| family);
        if !lash_kernel_lib::KERNEL_NAME_FAMILIES.contains(&family) {
            return Err(format!(
                "kernel helper `{name}` is outside the declared families"
            ));
        }
    }
    Ok(names)
}

/// The kernel families are recorded from its registry, alongside the closed enum.
pub fn kernel_source(registry: &FunctionRegistry) -> Result<String, String> {
    let names = kernel_names(registry)?;
    let families: BTreeSet<_> = names
        .iter()
        .map(|name| name.split_once('.').map_or("", |(prefix, _)| prefix))
        .collect();
    let mut source = enum_source("KernelHelper", names)?;
    source.push_str("\n/// Name families present in the registered kernel library.\npub const KERNEL_NAME_FAMILIES: &[&str] = &[\n");
    for family in families {
        source.push_str(&format!("    {family:?},\n"));
    }
    source.push_str("];\n");
    Ok(source)
}
