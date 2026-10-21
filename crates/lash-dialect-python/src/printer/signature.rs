//! A declared function's parameter list when captions name its parameters
//! or the fields of its types: one parameter a line, each below its `#:`
//! lines, an annotation for each parameter and result with a captioned
//! field, and a `TypedDict` for each record those annotations hold, written
//! from the entry's signature above every function.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{self as k, CaptionTarget, FieldPath, FieldRoot, FieldTarget, ParamTarget};
use lash_vocabulary::{Name, Type};

use super::{Renderer, names::attribute_identifier};

/// The annotations the functions' signatures are written with.
#[derive(Default)]
pub(super) struct Signatures {
    /// The functions written with one parameter a line.
    signed: BTreeSet<Name>,
    /// Each annotated parameter's and result's annotation.
    annotations: BTreeMap<(Name, FieldRoot), String>,
}

/// A `TypedDict` to write: one record of an annotated type.
struct Class {
    name: String,
    fields: Vec<ClassField>,
    rest: Option<String>,
}

struct ClassField {
    name: String,
    optional: bool,
    annotation: String,
    target: FieldTarget,
}

/// Names `TypedDict`s apart from every name the program writes.
struct Classes {
    taken: BTreeSet<String>,
    written: Vec<Class>,
}

impl Renderer<'_> {
    /// Writes the `TypedDict`s of every annotation a signature takes, and
    /// answers the signatures.
    ///
    /// An authored entry is annotated with every type its signature gives
    /// a parameter or its result, as its author wrote them.
    pub(super) fn typed_dicts(
        &mut self,
        document: &k::Document,
        authored: &BTreeSet<Name>,
    ) -> Signatures {
        let mut signatures = Signatures::default();
        let mut classes = Classes {
            taken: self
                .plan
                .names
                .values()
                .chain(self.plan.helpers.values())
                .cloned()
                .chain(
                    [
                        "Any",
                        "Literal",
                        "NotRequired",
                        "TypedDict",
                        "Exception",
                        "BaseException",
                    ]
                    .map(str::to_owned),
                )
                .collect(),
            written: Vec::new(),
        };
        for (name, export) in &document.exports {
            let k::Export::Function(entry) = export else {
                continue;
            };
            let Some(function) = document.functions.get(&entry.function) else {
                continue;
            };
            let params = function.signature.params.iter().map(|p| &p.name);
            let entry = Some(entry);
            // An entry's captions name its signature's parameters, which
            // the printed function takes only when its own are those.
            let spelled = entry.is_none_or(|entry| {
                entry
                    .signature
                    .params
                    .iter()
                    .map(|param| &param.name)
                    .eq(params)
            });
            let captioned = spelled
                && self
                    .out
                    .holds_caption(|target| target.function() == Some(name));
            let annotated = authored.contains(name) && spelled;
            if captioned {
                signatures.signed.insert(name.clone());
            }
            let Some(entry) = entry.filter(|_| captioned || annotated) else {
                continue;
            };
            let roots = entry
                .signature
                .params
                .iter()
                .map(|param| (FieldRoot::Param(param.name.clone()), &param.ty))
                .chain(std::iter::once((
                    FieldRoot::Result,
                    &entry.signature.result,
                )));
            for (root, ty) in roots {
                let annotated = (annotated && *ty != Type::Any)
                    || self.out.holds_caption(|target| {
                        matches!(target, CaptionTarget::Field(field)
                            if field.function == *name && field.path.root == root)
                    });
                if annotated {
                    let annotation = classes.annotation(ty, name, &root, &mut Vec::new());
                    signatures
                        .annotations
                        .insert((name.clone(), root), annotation);
                }
            }
        }
        for class in classes.written {
            self.class(&class);
        }
        signatures
    }

    fn class(&mut self, class: &Class) {
        self.out.write("class ");
        self.out.write(&class.name);
        self.out.write("(TypedDict");
        if let Some(rest) = &class.rest {
            self.out.write(", extra_items=");
            self.out.write(rest);
        }
        self.out.write("):\n");
        self.indent += 1;
        if class.fields.is_empty() {
            self.padding();
            self.out.write("pass\n");
        }
        for field in &class.fields {
            if let Some(caption) = self.out.take_caption(field.target.clone()) {
                self.spell(&caption);
            }
            self.padding();
            self.out.write(&field.name);
            self.out.write(": ");
            if field.optional {
                self.out.write("NotRequired[");
                self.out.write(&field.annotation);
                self.out.write("]");
            } else {
                self.out.write(&field.annotation);
            }
            self.out.write("\n");
        }
        self.indent -= 1;
    }

    /// Writes the parameters of `function`, declared as `name`, and the
    /// annotation of its result when it has one.
    pub(super) fn signature(&mut self, signatures: &Signatures, name: &Name, params: &[Name]) {
        if !signatures.signed.contains(name) {
            self.out.write("(");
            for (i, param) in params.iter().enumerate() {
                if i > 0 {
                    self.out.write(", ");
                }
                self.name(param);
                if let Some(annotation) = signatures
                    .annotations
                    .get(&(name.clone(), FieldRoot::Param(param.clone())))
                {
                    self.out.write(": ");
                    self.out.write(annotation);
                }
            }
            self.out.write(")");
            self.result_annotation(signatures, name);
            return;
        }
        self.out.write("(\n");
        self.indent += 1;
        for param in params {
            if let Some(caption) = self.out.take_caption(ParamTarget {
                function: name.clone(),
                param: param.clone(),
            }) {
                self.spell(&caption);
            }
            self.padding();
            self.name(param);
            if let Some(annotation) = signatures
                .annotations
                .get(&(name.clone(), FieldRoot::Param(param.clone())))
            {
                self.out.write(": ");
                self.out.write(annotation);
            }
            self.out.write(",\n");
        }
        self.indent -= 1;
        self.out.write(")");
        self.result_annotation(signatures, name);
    }

    fn result_annotation(&mut self, signatures: &Signatures, name: &Name) {
        if let Some(annotation) = signatures
            .annotations
            .get(&(name.clone(), FieldRoot::Result))
        {
            self.out.write(" -> ");
            self.out.write(annotation);
        }
    }
}

impl Classes {
    /// `ty` as a Python annotation, each record a `TypedDict` written
    /// before any that holds it. A record with a field no class body can
    /// name is a `dict`, and its fields' captions stay unspelled.
    fn annotation(
        &mut self,
        ty: &Type,
        function: &Name,
        root: &FieldRoot,
        path: &mut Vec<String>,
    ) -> String {
        match ty {
            Type::Any
            | Type::Json
            | Type::Handle(_)
            | Type::Function(_)
            | Type::Task(_)
            | Type::Timestamp => "Any".into(),
            Type::Null | Type::Absent => "None".into(),
            Type::Bool => "bool".into(),
            Type::Int => "int".into(),
            Type::Float | Type::Number => "float".into(),
            Type::Text => "str".into(),
            Type::Bytes => "bytes".into(),
            Type::Error => "Exception".into(),
            Type::Enum(members) if members.is_empty() => "Any".into(),
            Type::Enum(members) => format!(
                "Literal[{}]",
                members
                    .iter()
                    .map(|member| serde_json::Value::from(member.as_str()).to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Type::Tuple(members) if members.is_empty() => "tuple[()]".into(),
            Type::Tuple(members) => {
                let members: Vec<_> = members
                    .iter()
                    .map(|member| self.annotation(member, function, root, path))
                    .collect();
                format!("tuple[{}]", members.join(", "))
            }
            Type::List(item) => format!("list[{}]", self.annotation(item, function, root, path)),
            Type::Set(item) => format!("set[{}]", self.annotation(item, function, root, path)),
            Type::Map(map) => {
                let key = self.annotation(&map.key, function, root, path);
                let value = self.annotation(&map.value, function, root, path);
                format!("dict[{key}, {value}]")
            }
            Type::Union(members) => {
                let mut written: Vec<String> = Vec::new();
                for member in members {
                    let member = self.annotation(member, function, root, path);
                    if !written.contains(&member) {
                        written.push(member);
                    }
                }
                written.join(" | ")
            }
            Type::Record(record) => {
                if !record
                    .fields
                    .iter()
                    .all(|field| attribute_identifier(&field.name))
                {
                    return "dict[str, Any]".into();
                }
                let fields = record
                    .fields
                    .iter()
                    .map(|field| {
                        path.push(field.name.clone());
                        let annotation = self.annotation(&field.ty, function, root, path);
                        let target = FieldTarget {
                            function: function.clone(),
                            path: FieldPath {
                                root: root.clone(),
                                fields: path.clone(),
                            },
                        };
                        path.pop();
                        ClassField {
                            name: field.name.clone(),
                            optional: field.optional,
                            annotation,
                            target,
                        }
                    })
                    .collect();
                let rest = record
                    .rest
                    .as_deref()
                    .map(|rest| self.annotation(rest, function, root, path));
                let name = self.name(function, root, path);
                self.written.push(Class {
                    name: name.clone(),
                    fields,
                    rest,
                });
                name
            }
        }
    }

    /// A class name no other name of the program takes, made of the
    /// function's, the root's and the path's words.
    fn name(&mut self, function: &Name, root: &FieldRoot, path: &[String]) -> String {
        let root = match root {
            FieldRoot::Param(param) => param.as_str(),
            FieldRoot::Result => "result",
        };
        let mut base: String = std::iter::once(function.as_str())
            .chain(std::iter::once(root))
            .chain(path.iter().map(String::as_str))
            .flat_map(|word| word.split('_'))
            .map(|word| {
                let mut characters = word.chars();
                characters.next().map_or_else(String::new, |first| {
                    first.to_uppercase().chain(characters).collect()
                })
            })
            .collect();
        if !attribute_identifier(&base) {
            base = format!("T{base}");
        }
        if !attribute_identifier(&base) {
            base = "Record".into();
        }
        let mut name = base.clone();
        let mut suffix = 2;
        while !self.taken.insert(name.clone()) {
            name = format!("{base}{suffix}");
            suffix += 1;
        }
        name
    }
}
