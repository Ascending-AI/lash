//! A host package's build: its sources defined against the exact
//! artifacts it requires, checked and frozen into one artifact.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_kernel_dialect::{
    Export, Member, MemberPath, NamedLibrary, PackagePath, Segment, define_functions,
    define_functions_with,
};
use lash_kernel_doc::{
    ActKind, Action, ArtifactDigest, Expr, Formula, FunctionDefinition, FunctionId, FunctionName,
    FunctionRegistry, Implementation, Node, Operand, Type, parse_definition,
};

use crate::lash::built_definition;
use crate::package::{Code, PackageDisplay, digest_of};
use crate::shape::{STALE_VALUE, Shapes};
use crate::surface;
use crate::{NameRole, Origin, Package, PackageRefusal, Root};

/// A host package's three source files.
#[derive(Clone, Copy, Debug, Default)]
pub struct PackageSources<'a> {
    /// `<pkg>.kernel`: definitions in the helper-source format.
    pub kernel: &'a str,
    /// `<pkg>.surface`: the exports, under canonical names.
    pub surface: &'a str,
    /// `<pkg>.labels.tsv`: a title per member path.
    pub labels: &'a str,
}

/// The kernel functions a generated check calls by name.
const CHECK_CALLS: &[&str] = &[
    "add",
    "bool.not",
    "brand.of",
    "eq",
    "error.new",
    "kind",
    "map.values",
    "record.contains",
    "record.get",
    "record.len",
    "same",
    "set.values",
    "text.concat",
    "tuple.len",
];

impl Package {
    /// Defines, checks and freezes the host package at `path` from
    /// `sources`, against `requires`, the exact artifacts its bodies call
    /// into. Its one root is its own path. For each value type it
    /// generates `<path>.<type>.check_shape`, each member's body begins by
    /// checking its package-typed slots, and `<path>.__built()` returns the
    /// artifact's digest, which a constructor stamps its values with.
    ///
    /// # Errors
    ///
    /// The refusals of §2.1: a non-canonical segment, a segment declared
    /// twice, a definition outside the path, a member with no definition
    /// or other slots than its definition's, a sync member that can wait,
    /// and an operation performed but not exported; [`PackageRefusal::Source`] for unreadable sources.
    pub fn build(
        path: &str,
        release: &str,
        sources: &PackageSources<'_>,
        requires: &[&Package],
    ) -> Result<Self, PackageRefusal> {
        let path = PackagePath::parse(path).map_err(|invalid| PackageRefusal::NotCanonical {
            role: NameRole::Path,
            text: invalid.text,
        })?;
        let exports = surface::exports(&path, sources.surface)?;
        let display = PackageDisplay {
            members: surface::labels(&path, &exports, sources.labels)?,
            definitions: BTreeMap::new(),
        };
        let requirements: BTreeMap<PackagePath, ArtifactDigest> = requires
            .iter()
            .map(|package| (package.path().clone(), package.digest()))
            .collect();
        let roots: BTreeSet<Root> = [Root::new(path.to_string())].into();
        let types: BTreeMap<String, &Type> = exports
            .iter()
            .filter_map(|(segment, export)| match export {
                Export::Type(ty) => Some((segment.to_string(), &ty.representation)),
                _ => None,
            })
            .collect();
        let type_names: BTreeSet<String> = types.keys().cloned().collect();
        let shapes = Shapes::new(&path, types);
        let source = |message: String| PackageRefusal::Source {
            package: path.clone(),
            message,
        };
        let order = shapes.order().map_err(source)?;
        let members = members_by_definition(&exports);
        let kernel = resolve_signatures(&path.to_string(), &type_names, sources.kernel);
        let pass =
            |built: &[u8]| -> Result<Vec<(FunctionId, Arc<FunctionDefinition>)>, PackageRefusal> {
                let mut registry = FunctionRegistry::declarations();
                for package in requires {
                    package
                        .code()
                        .register_into(&mut registry, &|_| None)
                        .map_err(source)?;
                }
                let mut library = NamedLibrary::from_registry(&registry)
                    .map_err(|error| source(error.to_string()))?;
                let mut code = Vec::new();
                let stamp = built_definition(&path, built);
                library
                    .insert(stamp.clone())
                    .map_err(|error| source(error.to_string()))?;
                code.push(stamp);
                let mut checks = uses(CHECK_CALLS.iter().copied().map(str::to_owned));
                checks.push_str(&format!("use {}\n", crate::built_function(&path)));
                for ty in &order {
                    checks.push_str(&format!("use {path}.{ty}.check_shape\n"));
                }
                for ty in &order {
                    checks.push_str(&shapes.definition(ty).lines.join("\n"));
                    checks.push('\n');
                }
                code.extend(
                    define_functions(&checks, &mut library)
                        .map_err(|error| source(format!("a generated shape check: {error}")))?,
                );
                code.extend(
                    define_functions_with(&kernel, &mut library, &mut |definition, library| {
                        if members.contains_key(&definition.name) {
                            check_slots(&shapes, definition, library, built)?;
                        }
                        Ok(())
                    })
                    .map_err(|error| source(error.to_string()))?,
                );
                Ok(code
                    .into_iter()
                    .map(|definition| (definition.identity(), Arc::new(definition)))
                    .collect())
            };
        let first = pass(&[])?;
        let ids: Vec<FunctionId> = first.iter().map(|(function, _)| *function).collect();
        let digest = digest_of(&path, Origin::Host, &requirements, &roots, &exports, &ids);
        let code = pass(digest.as_bytes())?;
        let package = Self {
            path,
            release: release.to_owned(),
            origin: Origin::Host,
            requires: requirements,
            roots,
            exports,
            code: Code::defined(code),
            display,
            digest,
        };
        package.check_roots()?;
        package.check_members()?;
        package.check_code()?;
        Ok(package)
    }

    /// PKG-SURFACE: every member names a definition of the package with
    /// exactly its slots; a sync member's definition never waits.
    fn check_members(&self) -> Result<(), PackageRefusal> {
        let asynchronous: BTreeSet<FunctionId> = members_by_definition(&self.exports)
            .into_iter()
            .filter(|(_, (_, member))| member.asynchronous)
            .filter_map(|(definition, _)| self.function(definition.as_str()))
            .collect();
        for (definition, (member_path, member)) in members_by_definition(&self.exports) {
            let at = self.code.position(definition.as_str()).ok_or_else(|| {
                PackageRefusal::NoDefinition {
                    package: self.path.clone(),
                    member: member_path.clone(),
                    definition: definition.clone(),
                }
            })?;
            let found = self
                .code
                .definition(at)
                .map_err(|message| PackageRefusal::Source {
                    package: self.path.clone(),
                    message,
                })?;
            if found.signature != member.signature {
                return Err(PackageRefusal::OtherSlots {
                    package: self.path.clone(),
                    member: member_path,
                    definition: definition.clone(),
                    member_signature: Box::new(member.signature.clone()),
                    definition_signature: Box::new(found.signature.clone()),
                });
            }
            if !member.asynchronous
                && found.body().is_some_and(|body| {
                    waits(Node::Block(&body.block))
                        || body
                            .functions
                            .keys()
                            .any(|callee| asynchronous.contains(callee))
                })
            {
                return Err(PackageRefusal::SyncMemberWaits {
                    package: self.path.clone(),
                    member: member_path,
                    definition: definition.clone(),
                });
            }
        }
        Ok(())
    }

    /// Every operation a body performs is one the package exports
    /// (PKG-OPS).
    fn check_code(&self) -> Result<(), PackageRefusal> {
        for at in 0..self.code.len() {
            let definition =
                self.code
                    .definition(at)
                    .map_err(|message| PackageRefusal::Source {
                        package: self.path.clone(),
                        message,
                    })?;
            let Some(body) = definition.body() else {
                continue;
            };
            for effect in body.effects.keys() {
                let exported = effect
                    .as_str()
                    .strip_prefix(&format!("{}.", self.path))
                    .and_then(|name| Segment::new(name).ok())
                    .is_some_and(|name| {
                        matches!(self.exports.get(&name), Some(Export::Operation(_)))
                    });
                if !exported {
                    return Err(PackageRefusal::UndeclaredOperation {
                        package: self.path.clone(),
                        definition: definition.name.clone(),
                        effect: effect.clone(),
                    });
                }
            }
        }
        Ok(())
    }
}

/// Every member of `exports`, by the definition that implements it.
fn members_by_definition(
    exports: &BTreeMap<Segment, Export>,
) -> BTreeMap<FunctionName, (MemberPath, &Member)> {
    let mut members = BTreeMap::new();
    for (segment, export) in exports {
        match export {
            Export::Function(member) => {
                members.insert(
                    member.definition.clone(),
                    (
                        MemberPath {
                            export: segment.clone(),
                            member: None,
                        },
                        member,
                    ),
                );
            }
            Export::Type(ty) => {
                for (name, member) in &ty.members {
                    let member = member.member();
                    members.insert(
                        member.definition.clone(),
                        (
                            MemberPath {
                                export: segment.clone(),
                                member: Some(name.clone()),
                            },
                            member,
                        ),
                    );
                }
            }
            Export::Operation(_) | Export::Error => {}
        }
    }
    members
}

/// `kernel` with each definition's signature line read with the package's
/// type names standing for values of those types.
fn resolve_signatures(path: &str, types: &BTreeSet<String>, kernel: &str) -> String {
    let mut out = String::with_capacity(kernel.len());
    for line in kernel.lines() {
        match line.strip_prefix("function ") {
            Some(rest) => {
                out.push_str("function ");
                out.push_str(&surface::resolve_types(path, types, rest));
            }
            None => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

/// `use` header lines for `names`.
fn uses(names: impl IntoIterator<Item = String>) -> String {
    names
        .into_iter()
        .map(|name| format!("use {name}\n"))
        .collect()
}

/// Makes a member's body begin with the checks of its package-typed
/// slots, charged for and declaring the errors they raise.
fn check_slots(
    shapes: &Shapes<'_>,
    definition: &mut FunctionDefinition,
    library: &NamedLibrary,
    built: &[u8],
) -> Result<(), String> {
    let inline = matches!(definition.implementation, Implementation::Both(_)).then_some(built);
    let Some((checks, slots)) = shapes.slots(&definition.signature, inline) else {
        return Ok(());
    };
    let (Implementation::Body(body) | Implementation::Both(body)) = &mut definition.implementation
    else {
        // A native-only member checks its arguments natively.
        return Ok(());
    };
    let params: Vec<String> = definition
        .signature
        .params
        .iter()
        .map(|param| format!("{}: Any", param.name))
        .collect();
    let mut text = format!(
        "function {}.__checks({}) -> Null\nkernel {}\ncharge 1\nbody {{\n{}\n  return null\n}}\n",
        definition.name,
        params.join(", "),
        definition.kernel,
        checks.lines.join("\n")
    );
    for name in CHECK_CALLS
        .iter()
        .map(|name| (*name).to_owned())
        .chain(checks.calls.iter().cloned())
        .collect::<BTreeSet<_>>()
    {
        if let Some(function) = lash_kernel_dialect::Library::resolve(library, &name) {
            text.push_str(&format!("use {name} = @{function}\n"));
        }
    }
    let prologue = parse_definition(&text).map_err(|error| format!("its slot checks: {error}"))?;
    let Some(prologue_body) = prologue.body() else {
        unreachable!("the slot checks have a body")
    };
    let mut block = prologue_body.block.clone();
    block.pop();
    let held = Node::Block(&block).functions_within();
    for (function, name) in &prologue_body.functions {
        if held.contains(function) {
            body.functions.insert(*function, name.clone());
        }
    }
    body.block.splice(0..0, block);
    definition.errors.insert("type_error".to_owned());
    definition.errors.insert(STALE_VALUE.to_owned());
    let mut charge = vec![definition.charge.clone()];
    for slot in slots {
        charge.push(Formula::DeepSize(Operand::Param(
            lash_kernel_doc::Name::new(slot),
        )));
    }
    definition.charge = Formula::Sum(charge);
    Ok(())
}

/// Whether code below `node` can wait: it performs, sleeps, joins, yields
/// or awaits.
fn waits(node: Node<'_>) -> bool {
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        let waits = match node {
            Node::Action(action) => matches!(
                action,
                Action::Perform { .. }
                    | Action::Sleep { .. }
                    | Action::Join { .. }
                    | Action::JoinMany { .. }
                    | Action::Yield
                    | Action::Await { .. }
            ),
            Node::Expr(Expr::Act(act)) => matches!(
                act.kind,
                ActKind::Perform { .. }
                    | ActKind::Sleep { .. }
                    | ActKind::Join
                    | ActKind::JoinMany(_)
                    | ActKind::Yield
                    | ActKind::Await(_)
            ),
            _ => false,
        };
        if waits {
            return true;
        }
        pending.extend(node.children());
    }
    false
}
