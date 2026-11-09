//! Source requests carry referenced namespaces and share repeated operation types.
use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_dialect::{NamespaceKind, NamespaceMember, Namespaces, OperationSignature};
use lash_kernel_value::names::{Name, PackagePath, Segment};
use lash_kernel_value::types::{Param, Signature};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::Request;

mod exports;
mod source;
mod types;
use types::{TypeNode, TypeTable};

impl Request {
    pub(super) fn restrict_namespaces(&mut self) {
        let (source, namespaces, typescript) = match self {
            Self::Lower {
                dialect,
                source,
                namespaces,
                ..
            }
            | Self::ToolCalls {
                dialect,
                source,
                namespaces,
                ..
            } if matches!(dialect.as_str(), "typescript" | "python") => {
                (source, namespaces, dialect.as_str() == "typescript")
            }
            Self::ImportWorkflow {
                language,
                source,
                namespaces,
                ..
            } => (
                source,
                namespaces,
                matches!(language, lash_kernel_doc::SourceLanguage::TypeScript),
            ),
            _ => return,
        };
        let Some(identifiers) = source::identifiers(source, typescript) else {
            return;
        };
        // An ordinary TypeScript await also awaits durable process handles.
        // Its implicit processes.await call need not appear in the source.
        let process_await = typescript && identifiers.contains("await");
        let reached = |path: &PackagePath| {
            let root = path.as_slice()[0].as_str();
            identifiers.contains(root) || process_await && root == "processes"
        };
        let packages = namespaces
            .packages()
            .iter()
            .filter(|(path, _)| reached(path))
            .map(|(path, members)| (path.clone(), members.clone()))
            .collect();
        let deferred = namespaces
            .deferred()
            .iter()
            .filter(|path| reached(path))
            .cloned()
            .collect();
        // Removing packages cannot introduce a path conflict or reserved root.
        if let Ok(selected) = Namespaces::new(packages, deferred, &|_| false) {
            *namespaces = selected;
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    types: Vec<TypeNode>,
    packages: BTreeMap<PackagePath, BTreeMap<Segment, Member>>,
    deferred: BTreeSet<PackagePath>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Member {
    Operation {
        visibility: lash_kernel_value::package::Visibility,
        params: Vec<Parameter>,
        result: usize,
        controls: lash_kernel_value::TurnControls,
        mode: lash_kernel_value::OperationMode,
    },
    Export(exports::Export),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameter {
    name: Name,
    ty: usize,
    optional: bool,
}

pub(super) fn serialize<S: Serializer>(
    namespaces: &Namespaces,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut types = TypeTable::default();
    let packages = namespaces
        .packages()
        .iter()
        .map(|(path, members)| {
            let members = members
                .iter()
                .map(|(name, member)| {
                    let member = match &member.kind {
                        NamespaceKind::Operation(operation) => Member::Operation {
                            visibility: member.visibility,
                            params: operation
                                .signature
                                .params
                                .iter()
                                .map(|p| Parameter {
                                    name: p.name.clone(),
                                    ty: types.intern(&p.ty),
                                    optional: p.optional,
                                })
                                .collect(),
                            result: types.intern(&operation.signature.result),
                            controls: operation.controls.clone(),
                            mode: operation.mode,
                        },
                        _ => Member::Export(
                            exports::Export::new(member, &mut types)
                                .unwrap_or_else(|| unreachable!("operations are encoded above")),
                        ),
                    };
                    (name.clone(), member)
                })
                .collect();
            (path.clone(), members)
        })
        .collect();
    let deferred = namespaces.deferred().clone();
    Wire {
        types: types.nodes,
        packages,
        deferred,
    }
    .serialize(serializer)
}

pub(super) fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Namespaces, D::Error> {
    use serde::de::Error as _;
    let wire = Wire::deserialize(deserializer)?;
    let types = types::Types::new::<D::Error>(wire.types)?;
    let mut remaining = lash_vm_protocol::DecodeLimits::standard().max_allocation_bytes;
    for member in wire.packages.values().flat_map(|members| members.values()) {
        let indices: Vec<_> = match member {
            Member::Operation { params, result, .. } => params
                .iter()
                .map(|p| p.ty)
                .chain(std::iter::once(*result))
                .collect(),
            Member::Export(export) => export.indices(),
        };
        for index in indices {
            types.charge::<D::Error>(index, &mut remaining)?;
        }
    }
    let packages = wire
        .packages
        .into_iter()
        .map(|(path, members)| {
            let members = members
                .into_iter()
                .map(|(name, member)| {
                    let member = match member {
                        Member::Operation {
                            visibility,
                            params,
                            result,
                            controls,
                            mode,
                        } => NamespaceMember {
                            visibility,
                            kind: NamespaceKind::Operation(OperationSignature {
                                signature: Signature {
                                    params: params
                                        .into_iter()
                                        .map(|p| Param {
                                            name: p.name,
                                            ty: types.get(p.ty),
                                            optional: p.optional,
                                        })
                                        .collect(),
                                    result: types.get(result),
                                },
                                controls,
                                mode,
                            }),
                        },
                        Member::Export(member) => member.expand(&types),
                    };
                    (name, member)
                })
                .collect();
            (path, members)
        })
        .collect();
    Namespaces::new(packages, wire.deferred, &|_| false).map_err(D::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_kernel_value::Type;

    /// Source pruning preserves offered/deferred resolution and escaped roots.
    #[test]
    fn source_requests_retain_referenced_namespaces_and_escaped_roots()
    -> Result<(), Box<dyn std::error::Error>> {
        let catalogue = PackagePath::parse("catalogue")?;
        let unrelated = PackagePath::parse("unrelated")?;
        let deferred = PackagePath::parse("deferred")?;
        let namespaces = Namespaces::new(
            [
                (catalogue.clone(), BTreeMap::new()),
                (unrelated.clone(), BTreeMap::new()),
            ]
            .into(),
            [deferred.clone()].into(),
            &|_| false,
        )?;
        let mut request = Request::Lower {
            dialect: "typescript".into(),
            source: "catalogue.read(); deferred.call();".into(),
            namespaces: namespaces.clone(),
            bindings: Default::default(),
            functions: Default::default(),
        };
        request.restrict_namespaces();
        let bytes = rmp_serde::to_vec_named(&request)?;
        let request = rmp_serde::from_slice::<Request>(&bytes)?;
        let Request::Lower {
            namespaces: selected,
            ..
        } = request
        else {
            unreachable!()
        };
        assert_eq!(
            selected.packages().keys().cloned().collect::<Vec<_>>(),
            vec![catalogue]
        );
        assert_eq!(selected.deferred(), &[deferred].into());
        // A failed read cannot prove any namespace unused. The worker owns
        // the syntax error, including names after the point the parser stops.
        let mut malformed = Request::ImportWorkflow {
            language: lash_kernel_doc::SourceLanguage::TypeScript,
            source: "catalogue.read(); export const broken = ; deferred.call();".into(),
            namespaces: namespaces.clone(),
            limits: None,
        };
        malformed.restrict_namespaces();
        let Request::ImportWorkflow {
            namespaces: retained,
            ..
        } = malformed
        else {
            unreachable!()
        };
        assert_eq!(retained, namespaces);
        for (language, source, referenced) in [
            (
                lash_kernel_doc::SourceLanguage::TypeScript,
                r#"import { read as fetch } from "\u0063atalogue"; fetch();"#,
                true,
            ),
            (
                lash_kernel_doc::SourceLanguage::TypeScript,
                r#"import * as sdk from "catalogue.nested"; sdk.read();"#,
                true,
            ),
            (
                lash_kernel_doc::SourceLanguage::TypeScript,
                "console.log(`${1} ${catalogue.read()}`);",
                true,
            ),
            (
                lash_kernel_doc::SourceLanguage::TypeScript,
                "console.log(`catalogue ${1}`);",
                false,
            ),
            (
                lash_kernel_doc::SourceLanguage::TypeScript,
                "const r = /'catalogue/; catalogue.read();",
                true,
            ),
            (
                lash_kernel_doc::SourceLanguage::TypeScript,
                "const r = /catalogue/;",
                false,
            ),
            (
                lash_kernel_doc::SourceLanguage::TypeScript,
                "const λ = 1; catalogue.read(λ);",
                true,
            ),
            (
                lash_kernel_doc::SourceLanguage::Python,
                "print(f'{1} {ｃatalogue.read()}')",
                true,
            ),
            (
                lash_kernel_doc::SourceLanguage::Python,
                "print(f'catalogue {1}')",
                false,
            ),
            (
                lash_kernel_doc::SourceLanguage::TypeScript,
                r"\u0063atalogue.read();",
                true,
            ),
            (
                lash_kernel_doc::SourceLanguage::TypeScript,
                "catalogue.read('Unicode …');",
                true,
            ),
            (
                lash_kernel_doc::SourceLanguage::TypeScript,
                r"catalogue.read('\n');",
                true,
            ),
            (
                lash_kernel_doc::SourceLanguage::TypeScript,
                "console.log('Unicode … catalogue');",
                false,
            ),
            (
                lash_kernel_doc::SourceLanguage::TypeScript,
                "// catalogue\nconsole.log('ok');",
                false,
            ),
            (
                lash_kernel_doc::SourceLanguage::Python,
                "ｃatalogue.read('Unicode …')",
                true,
            ),
            (
                lash_kernel_doc::SourceLanguage::Python,
                r"print('catalogue\n')",
                false,
            ),
            (
                lash_kernel_doc::SourceLanguage::Python,
                "# catalogue\nprint('Unicode …')",
                false,
            ),
        ] {
            let mut escaped = Request::ImportWorkflow {
                language,
                source: source.into(),
                namespaces: namespaces.clone(),
                limits: None,
            };
            escaped.restrict_namespaces();
            let Request::ImportWorkflow {
                namespaces: selected,
                ..
            } = escaped
            else {
                unreachable!()
            };
            assert_eq!(
                selected
                    .packages()
                    .contains_key(&PackagePath::parse("catalogue")?),
                referenced,
                "{language:?}: {source}"
            );
            assert!(
                !selected.packages().contains_key(&unrelated),
                "{language:?}: {source}"
            );
            assert!(selected.deferred().is_empty(), "{language:?}: {source}");
        }
        let processes = PackagePath::parse("processes")?;
        assert_eq!(
            lash_dialect_typescript::source_identifiers("await.read()"),
            ["await".to_owned(), "read".to_owned()].into(),
            "Script-goal spelling repairs must not leak placeholder identifiers"
        );
        for (source, implicit) in [("await value;", true), ("console.log('await …');", false)] {
            let mut request = Request::Lower {
                dialect: "typescript".into(),
                source: source.into(),
                namespaces: Namespaces::new(
                    [(processes.clone(), BTreeMap::new())].into(),
                    Default::default(),
                    &|_| false,
                )?,
                bindings: Default::default(),
                functions: Default::default(),
            };
            request.restrict_namespaces();
            let Request::Lower { namespaces, .. } = request else {
                unreachable!()
            };
            assert_eq!(
                namespaces.packages().contains_key(&processes),
                implicit,
                "{source}"
            );
        }
        Ok(())
    }

    /// A catalogue whose tools share record types many times over decodes:
    /// the decoder charges the trees it builds by what they allocate, which
    /// the expanded node count bounds, not by a serialized estimate of them
    /// (FIG-7690: Cenote's tool catalogue was refused at 64 MiB).
    #[test]
    fn shared_record_catalogues_decode_within_their_node_bound()
    -> Result<(), Box<dyn std::error::Error>> {
        use lash_kernel_value::{RecordRest, RecordType, RecordTypeField};
        let record = |ty: Type| {
            Type::Record(RecordType {
                fields: (0..8)
                    .map(|i| RecordTypeField {
                        name: format!("f{i}"),
                        ty: ty.clone(),
                        optional: false,
                    })
                    .collect(),
                rest: RecordRest::Closed,
            })
        };
        // Nodes: block 1 + 8 = 9, step 1 + 8 * 9 = 73, doc 1 + 8 * 73 = 585.
        let doc = record(record(record(Type::Text)));
        let operations = 100;
        // Each operation takes a doc and returns a list of docs.
        let nodes = operations * (585 + 1 + 585);
        let path = PackagePath::parse("catalogue")?;
        let members = (0..operations)
            .map(|i| {
                Ok((
                    Segment::new(format!(
                        "read{}{}",
                        char::from(b'a' + (i / 26) as u8),
                        char::from(b'a' + (i % 26) as u8)
                    ))?,
                    NamespaceMember {
                        visibility: lash_kernel_value::Visibility::Public,
                        kind: NamespaceKind::Operation(OperationSignature {
                            signature: Signature {
                                params: vec![Param {
                                    name: Name::new("input"),
                                    ty: doc.clone(),
                                    optional: false,
                                }],
                                result: Type::List(Box::new(doc.clone())),
                            },
                            controls: Default::default(),
                            mode: lash_kernel_value::OperationMode::Read,
                        }),
                    },
                ))
            })
            .collect::<Result<_, Box<dyn std::error::Error>>>()?;
        let namespaces = Namespaces::new([(path, members)].into(), Default::default(), &|_| false)?;
        let request = Request::Lower {
            dialect: "typescript".into(),
            source: String::new(),
            namespaces: namespaces.clone(),
            bindings: Default::default(),
            functions: Default::default(),
        };
        let bytes =
            rmp_serde::to_vec_named(&lash_kernel_value::json_number::MessagePack(&request))?;
        let Request::Lower {
            namespaces: decoded,
            ..
        } = lash_kernel_value::json_number::decode::<Request>(&bytes)?
        else {
            unreachable!()
        };
        assert_eq!(decoded, namespaces);
        // Every node is one slot of its parent's container (a record field
        // at most) plus at most two allocations of its own: its field list,
        // and its field name of two bytes.
        let per_node = (size_of::<lash_kernel_value::RecordTypeField>() + 2 * 16 + 2) as u64;
        let wire: Wire =
            serde_json::from_value(serde_json::to_value(&request)?["lower"]["namespaces"].clone())?;
        let types = types::Types::new::<serde::de::value::Error>(wire.types)?;
        let mut remaining = u64::MAX;
        for member in wire.packages.values().flat_map(BTreeMap::values) {
            let Member::Operation { params, result, .. } = member else {
                unreachable!()
            };
            for index in params.iter().map(|p| p.ty).chain([*result]) {
                types.charge::<serde::de::value::Error>(index, &mut remaining)?;
            }
        }
        let charged = u64::MAX - remaining;
        assert!(
            charged <= nodes as u64 * per_node,
            "{charged} bytes charged for {nodes} nodes"
        );
        Ok(())
    }

    /// Sharing cannot amplify a small source frame beyond the allocation bound.
    #[test]
    fn shared_signature_references_cannot_amplify_the_decode_bound()
    -> Result<(), Box<dyn std::error::Error>> {
        use lash_kernel_dialect::{FunctionMember, TypeMember, ValueType};
        use lash_kernel_value::{MapType, RecordRest, RecordType, RecordTypeField};
        let repeated = Type::Record(RecordType {
            fields: vec![RecordTypeField {
                name: "shared".into(),
                ty: Type::Enum(vec!["a".into(), "b".into()]),
                optional: true,
            }],
            rest: RecordRest::Keep(Box::new(Type::Json)),
        });
        let signature = Signature {
            params: vec![Param {
                name: Name::new("input"),
                optional: false,
                ty: Type::Tuple(vec![
                    repeated.clone(),
                    Type::List(Box::new(repeated.clone())),
                    Type::Set(Box::new(repeated.clone())),
                    Type::Map(Box::new(MapType {
                        key: Type::Text,
                        value: repeated.clone(),
                    })),
                    Type::Task(Box::new(repeated.clone())),
                    Type::Union(vec![repeated.clone(), Type::Null]),
                ]),
            }],
            result: Type::Function(Box::new(Signature {
                params: vec![Param {
                    name: Name::new("value"),
                    ty: repeated.clone(),
                    optional: true,
                }],
                result: repeated.clone(),
            })),
        };
        let path = PackagePath::parse("catalogue")?;
        let function = FunctionMember {
            name: lash_kernel_value::QualifiedName {
                package: path.clone(),
                member: Segment::new("function")?,
                type_member: None,
            },
            convention: lash_kernel_value::Convention::Direct,
            signature: signature.clone(),
            asynchronous: true,
            rest: false,
        };
        let visible = |kind| NamespaceMember {
            visibility: lash_kernel_value::Visibility::Public,
            kind,
        };
        let namespaces = Namespaces::new(
            [(
                path,
                [
                    (
                        Segment::new("operation")?,
                        visible(NamespaceKind::Operation(OperationSignature {
                            signature,
                            controls: Default::default(),
                            mode: lash_kernel_value::OperationMode::Read,
                        })),
                    ),
                    (
                        Segment::new("function")?,
                        visible(NamespaceKind::Function(function.clone())),
                    ),
                    (
                        Segment::new("value")?,
                        visible(NamespaceKind::Type(ValueType {
                            representation: repeated,
                            members: [
                                (
                                    Segment::new("static")?,
                                    TypeMember::Static(function.clone()),
                                ),
                                (
                                    Segment::new("method")?,
                                    TypeMember::Method(function.clone()),
                                ),
                                (Segment::new("property")?, TypeMember::Property(function)),
                            ]
                            .into(),
                        })),
                    ),
                    (Segment::new("error")?, visible(NamespaceKind::Error)),
                ]
                .into(),
            )]
            .into(),
            Default::default(),
            &|_| false,
        )?;
        let original = Request::ImportWorkflow {
            language: lash_kernel_doc::SourceLanguage::TypeScript,
            source: String::new(),
            namespaces: namespaces.clone(),
            limits: None,
        };
        let values = serde_json::to_value(&original)?;
        assert_eq!(
            values["import_workflow"]["namespaces"]["types"]
                .as_array()
                .ok_or("type table")?
                .len(),
            12,
            "all 12 distinct types, across operation and export signatures, are shipped once"
        );
        let Request::ImportWorkflow {
            namespaces: restored,
            ..
        } = rmp_serde::from_slice(&rmp_serde::to_vec_named(&original)?)?
        else {
            unreachable!()
        };
        assert_eq!(restored, namespaces);
        let mut amplified = vec![TypeNode::Leaf(Type::Text)];
        for index in 0..30 {
            amplified.push(TypeNode::Tuple(vec![index, index]));
        }
        let mut remaining = lash_vm_protocol::DecodeLimits::standard().max_allocation_bytes;
        let error = types::Types::new::<serde::de::value::Error>(amplified)?
            .charge::<serde::de::value::Error>(30, &mut remaining)
            .err()
            .ok_or("nested amplification was accepted")?;
        assert!(
            error
                .to_string()
                .contains("decoded signatures exceed 64 MiB"),
            "{error}"
        );
        for node in [TypeNode::List(0), TypeNode::Tuple(vec![1])] {
            let error = types::Types::new::<serde::de::value::Error>(vec![node])
                .err()
                .ok_or("cyclic or unknown reference was accepted")?;
            assert!(
                error.to_string().contains("forward shared signature type"),
                "{error}"
            );
        }
        let mut request = serde_json::to_value(Request::ImportWorkflow {
            language: lash_kernel_doc::SourceLanguage::TypeScript,
            source: String::new(),
            namespaces: Namespaces::default(),
            limits: None,
        })?;
        let members = (0..100)
            .map(|i| {
                Ok((
                    Segment::new(format!(
                        "read{}{}",
                        char::from(b'a' + i / 26),
                        char::from(b'a' + i % 26)
                    ))?,
                    Member::Operation {
                        visibility: lash_kernel_value::package::Visibility::Public,
                        params: Vec::new(),
                        result: 0,
                        controls: Default::default(),
                        mode: lash_kernel_value::OperationMode::Read,
                    },
                ))
            })
            .collect::<Result<_, lash_kernel_value::names::InvalidSegment>>()?;
        request["import_workflow"]["namespaces"] = serde_json::to_value(Wire {
            types: vec![TypeNode::Leaf(Type::Enum(vec!["x".repeat(1024 * 1024)]))],
            packages: [(PackagePath::parse("catalogue")?, members)].into(),
            deferred: Default::default(),
        })?;
        let bytes =
            rmp_serde::to_vec_named(&lash_kernel_value::json_number::MessagePack(&request))?;
        lash_vm_protocol::FrameCodec::new(lash_vm_protocol::DecodeLimits::standard())
            .check_payload(&bytes)?;
        let Err(error) = lash_kernel_value::json_number::decode::<Request>(&bytes) else {
            return Err("shared types exceeded the expansion bound".into());
        };
        assert!(
            error
                .to_string()
                .contains("decoded signatures exceed 64 MiB"),
            "{error}"
        );
        Ok(())
    }
}
