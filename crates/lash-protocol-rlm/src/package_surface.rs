//! What the prompt shows of Current (§2.3, P3): each host
//! package's code exports under its path, in the session dialect's
//! spelling. Its operations are the tool declarations the prompt already
//! shows; lash's own packages export nothing and are not listed.

use std::fmt::Write as _;

use lash_kernel_dialect::{
    BRAND_FIELD, NamespaceKind, Namespaces, SpellKind, Spelling, TypeMember,
};
use lash_vocabulary::{Signature, Type};

pub(crate) fn render(view: &Namespaces, spelling: Spelling) -> String {
    let mut out = String::new();
    for (package, members) in view.packages() {
        let path = package
            .iter()
            .map(|segment| spelling.spell(SpellKind::PathSegment, segment))
            .collect::<Vec<_>>()
            .join(".");
        let mut lines = Vec::new();
        for (segment, member) in members {
            if member.visibility != lash_vocabulary::Visibility::Public {
                continue;
            }
            match &member.kind {
                NamespaceKind::Operation(_) => {}
                NamespaceKind::Function(member) => lines.push(format!(
                    "`{path}.{}{}`",
                    spelling.spell(SpellKind::Member, segment),
                    call(&member.signature, 0, spelling)
                )),
                NamespaceKind::Type(value_type) => {
                    let ty = format!("{path}.{}", spelling.spell(SpellKind::TypeName, segment));
                    lines.push(format!("`{ty}`: a sealed value type"));
                    for (name, member) in &value_type.members {
                        let name = spelling.spell(SpellKind::Member, name);
                        lines.push(match member {
                            TypeMember::Static(member) => {
                                format!("`{ty}.{name}{}`", call(&member.signature, 0, spelling))
                            }
                            TypeMember::Method(member) => {
                                format!("`{ty}#{name}{}`", call(&member.signature, 1, spelling))
                            }
                            TypeMember::Property(member) => format!(
                                "`{ty}#{name}: {}`",
                                type_text(&member.signature.result, spelling)
                            ),
                        });
                    }
                }
                NamespaceKind::Error => lines.push(format!(
                    "`{path}.{}`: an error",
                    spelling.spell(SpellKind::TypeName, segment)
                )),
            }
        }
        if lines.is_empty() {
            continue;
        }
        let _ = writeln!(out, "#### `{path}`\n");
        for line in lines {
            let _ = writeln!(out, "- {line}");
        }
        out.push('\n');
    }
    out.trim_end().to_owned()
}

/// `signature`'s parameters after the first `skip` and its result.
fn call(signature: &Signature, skip: usize, spelling: Spelling) -> String {
    let params = signature
        .params
        .iter()
        .skip(skip)
        .map(|param| format!("{}: {}", param.name, type_text(&param.ty, spelling)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("({params}) -> {}", type_text(&signature.result, spelling))
}

/// `ty` as the prompt shows it: a package value by its type's spelled
/// path, everything else in kernel type notation.
fn type_text(ty: &Type, spelling: Spelling) -> String {
    match ty {
        Type::Record(record) => match branded(record) {
            Some((path, name)) => {
                let path = path
                    .split('.')
                    .map(|segment| spelled(spelling, SpellKind::PathSegment, segment))
                    .collect::<Vec<_>>()
                    .join(".");
                format!("{path}.{}", spelled(spelling, SpellKind::TypeName, name))
            }
            None => {
                let mut fields = record
                    .fields
                    .iter()
                    .map(|field| {
                        format!(
                            "{}{}: {}",
                            field.name,
                            if field.optional { "?" } else { "" },
                            type_text(&field.ty, spelling)
                        )
                    })
                    .collect::<Vec<_>>();
                if let Some(rest) = &record.rest {
                    fields.push(format!("..{}", type_text(rest, spelling)));
                }
                format!("Record{{{}}}", fields.join(", "))
            }
        },
        Type::List(item) => format!("List({})", type_text(item, spelling)),
        Type::Set(item) => format!("Set({})", type_text(item, spelling)),
        Type::Tuple(items) => format!(
            "Tuple({})",
            items
                .iter()
                .map(|item| type_text(item, spelling))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Type::Union(members) => members
            .iter()
            .map(|member| type_text(member, spelling))
            .collect::<Vec<_>>()
            .join(" | "),
        Type::Function(signature) => format!("Fn{}", call(signature, 0, spelling)),
        other => lash_vocabulary::print_type(other),
    }
}

/// The `(package, type)` a package value's record type names.
fn branded(record: &lash_vocabulary::RecordType) -> Option<(&str, &str)> {
    let field = record
        .fields
        .iter()
        .find(|field| field.name == BRAND_FIELD)?;
    let Type::Tuple(parts) = &field.ty else {
        return None;
    };
    match parts.as_slice() {
        [Type::Enum(path), Type::Enum(name)] => match (path.as_slice(), name.as_slice()) {
            ([path], [name]) => Some((path, name)),
            _ => None,
        },
        _ => None,
    }
}

fn spelled(spelling: Spelling, kind: SpellKind, text: &str) -> String {
    lash_vocabulary::names::Segment::new(text).map_or_else(
        |_| text.to_owned(),
        |segment| spelling.spell(kind, &segment),
    )
}
