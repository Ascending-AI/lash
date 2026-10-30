//! Source requests carry referenced namespaces and share repeated operation types.
use std::collections::{BTreeMap, BTreeSet, HashMap};

use lash_kernel_dialect::{NamespaceKind, NamespaceMember, Namespaces, OperationSignature};
use lash_kernel_value::names::{Name, PackagePath, Segment};
use lash_kernel_value::types::{Param, Signature, Type};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::Request;

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
        // Path roots have identical verbatim spellings in both dialects. Keep
        // whole namespaces, including exports and misspelling diagnostics. An
        // escape or Python NFKC normalization may spell a root without its
        // literal bytes, so retain all for escaped or non-ASCII source.
        if source.contains('\\') || !source.is_ascii() {
            return;
        }
        // An ordinary TypeScript await also awaits durable process handles.
        // Its implicit processes.await call need not appear in the source.
        let process_await = typescript && source.contains("await");
        let reached = |path: &PackagePath| {
            let root = path.as_slice()[0].as_str();
            source.contains(root) || process_await && root == "processes"
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
    types: Vec<Type>,
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
    Export(NamespaceMember),
}

#[derive(Serialize, Deserialize)]
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
    let mut types = Vec::new();
    let mut indices = HashMap::<&Type, usize>::new();
    fn intern<'a>(
        ty: &'a Type,
        types: &mut Vec<Type>,
        indices: &mut HashMap<&'a Type, usize>,
    ) -> usize {
        if let Some(index) = indices.get(ty) {
            return *index;
        }
        let index = types.len();
        types.push(ty.clone());
        indices.insert(ty, index);
        index
    }
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
                                    ty: intern(&p.ty, &mut types, &mut indices),
                                    optional: p.optional,
                                })
                                .collect(),
                            result: intern(&operation.signature.result, &mut types, &mut indices),
                            controls: operation.controls.clone(),
                            mode: operation.mode,
                        },
                        _ => Member::Export(member.clone()),
                    };
                    (name.clone(), member)
                })
                .collect();
            (path.clone(), members)
        })
        .collect();
    let deferred = namespaces.deferred().clone();
    Wire {
        types,
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
    // Interning must not turn a bounded wire into unbounded cloned types.
    // Charge every expanded JSON value as a container element, plus strings.
    fn charge(value: &serde_json::Value) -> u64 {
        128 + match value {
            serde_json::Value::String(s) => s.len() as u64,
            serde_json::Value::Array(v) => v.iter().map(charge).sum(),
            serde_json::Value::Object(v) => v
                .iter()
                .map(|(k, v)| 128 + k.len() as u64 + charge(v))
                .sum(),
            _ => 0,
        }
    }
    let charges = wire
        .types
        .iter()
        .map(|ty| serde_json::to_value(ty).map(|v| charge(&v)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(D::Error::custom)?;
    let mut remaining = lash_vm_protocol::DecodeLimits::standard().max_allocation_bytes;
    for member in wire.packages.values().flat_map(|members| members.values()) {
        if let Member::Operation { params, result, .. } = member {
            for index in params.iter().map(|p| p.ty).chain(std::iter::once(*result)) {
                let size = charges
                    .get(index)
                    .ok_or_else(|| D::Error::custom("unknown shared signature type"))?;
                remaining = remaining
                    .checked_sub(*size)
                    .ok_or_else(|| D::Error::custom("expanded signatures exceed 64 MiB"))?;
            }
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
                                            ty: wire.types[p.ty].clone(),
                                            optional: p.optional,
                                        })
                                        .collect(),
                                    result: wire.types[result].clone(),
                                },
                                controls,
                                mode,
                            }),
                        },
                        Member::Export(member) => member,
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
        for source in [r"\u0063atalogue.read();", "\u{ff43}atalogue.read();"] {
            let mut escaped = Request::ImportWorkflow {
                language: lash_kernel_doc::SourceLanguage::TypeScript,
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
            assert_eq!(selected, namespaces);
        }
        Ok(())
    }

    /// Sharing cannot amplify a small source frame beyond the allocation bound.
    #[test]
    fn shared_signature_references_cannot_amplify_the_decode_bound()
    -> Result<(), Box<dyn std::error::Error>> {
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
            types: vec![Type::Enum(vec!["x".repeat(1024 * 1024)])],
            packages: [(PackagePath::parse("catalogue")?, members)].into(),
            deferred: Default::default(),
        })?;
        let bytes = rmp_serde::to_vec_named(&lash_kernel_value::json_number::Native(&request))?;
        lash_vm_protocol::FrameCodec::new(lash_vm_protocol::DecodeLimits::standard())
            .check_payload(&bytes)?;
        let Err(error) = rmp_serde::from_slice::<Request>(&bytes) else {
            return Err("shared types exceeded the expansion bound".into());
        };
        assert!(
            error
                .to_string()
                .contains("expanded signatures exceed 64 MiB"),
            "{error}"
        );
        Ok(())
    }
}
