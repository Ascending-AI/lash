//! What the prompt shows of the cell's package set (§2.3, P3): each host
//! package's code exports under its path, in the session dialect's
//! spelling. Its operations are the tool declarations the prompt already
//! shows; lash's own packages export nothing and are not listed.

use std::fmt::Write as _;

use lash_kernel_dialect::{BRAND_FIELD, Export, SpellKind, Spelling, TypeMember};
use lash_kernel_package::{Origin, PackageView};
use lash_vocabulary::types::Signature;
use lash_vocabulary::types::Type;

/// The code exports of `view`'s host packages, each package under a
/// heading of its path and release; empty when the set holds none.
pub(crate) fn render(view: &PackageView<'_>, spelling: Spelling) -> String {
    let mut out = String::new();
    for package in view
        .packages()
        .filter(|package| package.origin() == Origin::Host)
    {
        let path = package
            .path()
            .iter()
            .map(|segment| spelling.spell(SpellKind::PathSegment, segment))
            .collect::<Vec<_>>()
            .join(".");
        let mut lines = Vec::new();
        for (segment, export) in package.exports() {
            match export {
                Export::Operation(_) => {}
                Export::Function(member) => lines.push(format!(
                    "`{path}.{}{}`",
                    spelling.spell(SpellKind::Member, segment),
                    call(&member.signature, 0, spelling)
                )),
                Export::Type(value_type) => {
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
                Export::Error => lines.push(format!(
                    "`{path}.{}`: an error",
                    spelling.spell(SpellKind::TypeName, segment)
                )),
            }
        }
        if lines.is_empty() {
            continue;
        }
        let _ = writeln!(out, "#### `{path}` ({})\n", package.release());
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
fn branded(record: &lash_vocabulary::types::RecordType) -> Option<(&str, &str)> {
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
