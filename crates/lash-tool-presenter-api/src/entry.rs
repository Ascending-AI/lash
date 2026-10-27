//! What the prompt shows of Current (§2.3, P3): each host
//! package's code exports under its path, in the session dialect's
//! spelling. Its operations are the tool declarations the prompt already
//! shows; lash's own packages export nothing and are not listed.

use std::fmt::Write as _;

use lash_kernel_dialect::{
    BRAND_FIELD, NamespaceKind, Namespaces, SpellKind, Spelling, TypeMember,
};
use {lash_kernel_value::Signature, lash_kernel_value::Type};
/// Canonical dialect spelling used by the TypeScript presenter.
pub const TYPESCRIPT_SPELLING: Spelling = Spelling::TypeScript;
/// Canonical dialect spelling used by the Python presenter.
pub const PYTHON_SPELLING: Spelling = Spelling::Python;

pub fn render_package_surface(view: &Namespaces, spelling: Spelling) -> String {
    let mut out = String::new();
    for (package, members) in view.packages() {
        let path = package
            .iter()
            .map(|segment| spelling.spell(SpellKind::PathSegment, segment))
            .collect::<Vec<_>>()
            .join(".");
        let mut lines = Vec::new();
        for (segment, member) in members {
            if member.visibility != lash_kernel_value::Visibility::Public {
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

/// Render a public entry signature, optionally omitting a method receiver.
pub fn render_entry_signature(signature: &Signature, skip: usize, spelling: Spelling) -> String {
    call(signature, skip, spelling)
}

fn call(signature: &Signature, skip: usize, spelling: Spelling) -> String {
    let params = signature
        .params
        .iter()
        .skip(skip)
        .map(|param| {
            let name = param.name.as_str();
            let ty = type_text(&param.ty, spelling);
            match spelling {
                Spelling::TypeScript => {
                    format!("{name}{}: {ty}", if param.optional { "?" } else { "" })
                }
                Spelling::Python => {
                    format!("{name}: {ty}{}", if param.optional { " = ..." } else { "" })
                }
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let result = type_text(&signature.result, spelling);
    match spelling {
        Spelling::TypeScript => format!("({params}): {result}"),
        Spelling::Python => format!("({params}) -> {result}"),
    }
}

/// Render a type using source-language notation and canonical package brands.
pub fn render_entry_type(ty: &Type, spelling: Spelling) -> String {
    type_text(ty, spelling)
}

fn type_text(ty: &Type, spelling: Spelling) -> String {
    let ts = spelling == Spelling::TypeScript;
    let render = |ty: &Type| type_text(ty, spelling);
    match ty {
        Type::Record(record) if branded(record).is_some() => {
            let (path, name) = branded(record).unwrap_or(("", ""));
            let path = path
                .split('.')
                .map(|segment| spelled(spelling, SpellKind::PathSegment, segment))
                .collect::<Vec<_>>()
                .join(".");
            format!("{path}.{}", spelled(spelling, SpellKind::TypeName, name))
        }
        Type::Record(record) => {
            let mut fields = record
                .fields
                .iter()
                .map(|field| {
                    let name = serde_json::Value::String(field.name.clone());
                    let ty = render(&field.ty);
                    if ts {
                        format!("{name}{}: {ty}", if field.optional { "?" } else { "" })
                    } else {
                        format!(
                            "{name}: {}",
                            if field.optional {
                                format!("NotRequired[{ty}]")
                            } else {
                                ty
                            }
                        )
                    }
                })
                .collect::<Vec<_>>();
            if ts {
                match &record.rest {
                    lash_kernel_value::RecordRest::Closed => {
                        fields.push("[key: string]: never".into())
                    }
                    lash_kernel_value::RecordRest::Drop => {}
                    lash_kernel_value::RecordRest::Keep(rest) => {
                        fields.push(format!("[key: string]: {}", render(rest)))
                    }
                }
                format!("{{ {} }}", fields.join("; "))
            } else {
                let rest = match &record.rest {
                    lash_kernel_value::RecordRest::Closed => ", closed=True".into(),
                    lash_kernel_value::RecordRest::Drop => String::new(),
                    lash_kernel_value::RecordRest::Keep(rest) => {
                        format!(", extra_items={}", render(rest))
                    }
                };
                format!("TypedDict(\"Entry\", {{{}}}{rest})", fields.join(", "))
            }
        }
        Type::List(item) => {
            if ts {
                format!("Array<{}>", render(item))
            } else {
                format!("list[{}]", render(item))
            }
        }
        Type::Set(item) => {
            if ts {
                format!("Set<{}>", render(item))
            } else {
                format!("set[{}]", render(item))
            }
        }
        Type::Map(map) => {
            if ts {
                format!("Map<{}, {}>", render(&map.key), render(&map.value))
            } else {
                format!("dict[{}, {}]", render(&map.key), render(&map.value))
            }
        }
        Type::Tuple(items) => {
            let items = items.iter().map(render).collect::<Vec<_>>().join(", ");
            if ts {
                format!("[{items}]")
            } else if items.is_empty() {
                "tuple[()]".into()
            } else {
                format!("tuple[{items}]")
            }
        }
        Type::Union(members) => members.iter().map(render).collect::<Vec<_>>().join(" | "),
        Type::Enum(values) => {
            let values = values
                .iter()
                .map(|value| serde_json::Value::String(value.clone()).to_string())
                .collect::<Vec<_>>();
            if ts {
                values.join(" | ")
            } else {
                format!("Literal[{}]", values.join(", "))
            }
        }
        Type::Function(signature) => {
            if ts {
                format!(
                    "({}) => {}",
                    signature
                        .params
                        .iter()
                        .map(|param| format!("{}: {}", param.name, render(&param.ty)))
                        .collect::<Vec<_>>()
                        .join(", "),
                    render(&signature.result)
                )
            } else {
                format!(
                    "Callable[[{}], {}]",
                    signature
                        .params
                        .iter()
                        .map(|param| render(&param.ty))
                        .collect::<Vec<_>>()
                        .join(", "),
                    render(&signature.result)
                )
            }
        }
        Type::Any => if ts { "any" } else { "Any" }.into(),
        Type::Json => "JsonValue".into(),
        Type::Absent => if ts { "undefined" } else { "Absent" }.into(),
        Type::Null => if ts { "null" } else { "None" }.into(),
        Type::Bool => if ts { "boolean" } else { "bool" }.into(),
        Type::Int => if ts { "number" } else { "int" }.into(),
        Type::Number | Type::Float => if ts { "number" } else { "float" }.into(),
        Type::Text => if ts { "string" } else { "str" }.into(),
        Type::Bytes => if ts { "Uint8Array" } else { "bytes" }.into(),
        Type::Timestamp => "Timestamp".into(),
        Type::Task(item) => {
            if ts {
                format!("Promise<{}>", render(item))
            } else {
                format!("Awaitable[{}]", render(item))
            }
        }
        Type::Error => if ts { "Error" } else { "Exception" }.into(),
        Type::Handle(name) => name.clone(),
    }
}

/// Render an admission signature mismatch in the authored language.
pub fn render_signature_mismatch(
    mismatch: &lash_kernel_value::SignatureMismatch,
    spelling: Spelling,
) -> String {
    format!(
        "{} at {}: expected {}, provided {}",
        mismatch.side,
        mismatch.path,
        type_text(&mismatch.expected, spelling),
        type_text(&mismatch.provided, spelling),
    )
}

/// Render the entry annotation refusal; source coordinates remain available
/// on the owning diagnostic for hosts to present alongside this text.
pub fn render_signature_required(required: &lash_kernel_value::SignatureRequired) -> String {
    let function = required.function.name.as_deref().unwrap_or("<anonymous>");
    match &required.position {
        lash_kernel_value::SignaturePosition::Parameter { name, index } => format!(
            "SIGNATURE_REQUIRED: {function} parameter {index} ({name}) needs a written type annotation"
        ),
        lash_kernel_value::SignaturePosition::Result => {
            format!("SIGNATURE_REQUIRED: {function} needs a written settled-result type annotation")
        }
    }
}

/// Render the owning entry mismatch using this dialect's type spelling.
pub fn render_entry_type_mismatch(
    source: &lash_kernel_value::EntryTypeMismatch,
    spelling: Spelling,
) -> String {
    let callable = &source.callable;
    let lash_kernel_value::TypeMismatch {
        path,
        expected,
        actual,
    } = &source.mismatch;
    format!(
        "ENTRY_TYPE_MISMATCH: {callable:?} at {path}: expected {}, actual {}",
        type_text(expected, spelling),
        type_text(actual, spelling)
    )
}

/// The `(package, type)` a package value's record type names.
fn branded(record: &lash_kernel_value::RecordType) -> Option<(&str, &str)> {
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
    lash_kernel_value::names::Segment::new(text).map_or_else(
        |_| text.to_owned(),
        |segment| spelling.spell(kind, &segment),
    )
}

#[cfg(test)]
mod laws {
    use super::*;
    use {lash_kernel_value::Name, lash_kernel_value::Param};

    /// ENTRY-PRESENTATION: signature optionality and diagnostic facts retain
    /// their meanings in both source languages.
    #[test]
    fn entry_contract_facts_use_the_dialect_spelling() {
        for (rest, ts, py) in [
            (
                lash_kernel_value::RecordRest::Closed,
                "[key: string]: never",
                "closed=True",
            ),
            (lash_kernel_value::RecordRest::Drop, "", ""),
            (
                lash_kernel_value::RecordRest::Keep(Box::new(Type::Json)),
                "[key: string]: JsonValue",
                "extra_items=JsonValue",
            ),
        ] {
            let ty = Type::Record(lash_kernel_value::RecordType {
                fields: vec![],
                rest,
            });
            assert_eq!(
                render_entry_type(&ty, Spelling::TypeScript),
                format!("{{ {ts} }}")
            );
            let suffix = if py.is_empty() {
                String::new()
            } else {
                format!(", {py}")
            };
            assert_eq!(
                render_entry_type(&ty, Spelling::Python),
                format!("TypedDict(\"Entry\", {{}}{suffix})")
            );
        }
        let signature = Signature {
            params: vec![Param {
                name: Name::new("event"),
                ty: Type::Text,
                optional: true,
            }],
            result: Type::Bool,
        };
        assert_eq!(
            render_entry_signature(&signature, 0, Spelling::TypeScript),
            "(event?: string): boolean"
        );
        assert_eq!(
            render_entry_signature(&signature, 0, Spelling::Python),
            "(event: str = ...) -> bool"
        );
        assert_eq!(
            render_entry_type(&Type::Tuple(vec![]), Spelling::Python),
            "tuple[()]"
        );
        assert_eq!(
            render_signature_required(&lash_kernel_value::SignatureRequired {
                function: lash_kernel_value::FunctionLocation {
                    name: Some("entry".into()),
                    site: None
                },
                position: lash_kernel_value::SignaturePosition::Parameter {
                    name: Name::new("event"),
                    index: 0
                },
                span: None,
            }),
            "SIGNATURE_REQUIRED: entry parameter 0 (event) needs a written type annotation"
        );
        assert_eq!(
            render_signature_required(&lash_kernel_value::SignatureRequired {
                function: lash_kernel_value::FunctionLocation {
                    name: None,
                    site: None
                },
                position: lash_kernel_value::SignaturePosition::Result,
                span: None,
            }),
            "SIGNATURE_REQUIRED: <anonymous> needs a written settled-result type annotation"
        );
        let callable = lash_kernel_value::CallableId {
            link: lash_kernel_value::LinkId::from_bytes([1; 32]),
            function: lash_kernel_value::FunctionId::from_bytes([2; 32]),
        };
        for (spelling, expected, actual) in [
            (Spelling::TypeScript, "string", "number"),
            (Spelling::Python, "str", "float"),
        ] {
            let text = render_entry_type_mismatch(
                &lash_kernel_value::EntryTypeMismatch {
                    callable,
                    mismatch: lash_kernel_value::TypeMismatch {
                        path: "/output/id".into(),
                        expected: Type::Text,
                        actual: Type::Number,
                    },
                },
                spelling,
            );
            assert!(
                text.contains(&format!(
                    "at /output/id: expected {expected}, actual {actual}"
                )),
                "{text}"
            );
            assert!(text.contains(&callable.function.to_string()), "{text}");
        }
    }
}
