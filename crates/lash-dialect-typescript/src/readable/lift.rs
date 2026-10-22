//! Lifting: a kernel region back to the TypeScript a model would have
//! written for it. Each rule inverts one shape the front end lowers to
//! (`crate::lower`); a node no rule recognizes leaves its region unlifted.
//! Lifting only proposes: [`super::print`] keeps a region's lift only when
//! reading it back gives the region again.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{self as k, Caption, CaptionTarget, FieldPath, FieldRoot};

use lash_vocabulary::Name;
use lash_vocabulary::sites::{Site, Unit};

use super::source::{self, Item, Param, Property, Stmt, Type};

/// Why a region did not lift.
pub(super) type Lift<T> = Result<T, String>;

pub(super) fn refuse<T>(why: impl Into<String>) -> Lift<T> {
    Err(why.into())
}

/// What a document lifts to: each declared function on its own, and
/// `main` as the order of the module's entries or as a cell's statements.
pub(super) struct Lifted {
    pub(super) functions: BTreeMap<Name, (lash_vocabulary::FunctionId, Lift<Item>)>,
    pub(super) main: Lift<Main>,
}

pub(super) enum Main {
    /// A module: declarations interleaved with its executable statements.
    Module(Vec<ModulePart>),
    /// A cell: `main`'s statements.
    Cell(Vec<Stmt>),
}

pub(super) enum ModulePart {
    Export(Name),
    Statements(Vec<Stmt>),
}

pub(super) fn document(
    document: &k::Document,
    captions: Option<&k::Captions>,
    library: &dyn lash_kernel_dialect::Library,
) -> Lifted {
    let functions = document
        .exports
        .iter()
        .filter_map(|(name, export)| match export {
            k::Export::Function(export) => Some((
                name.clone(),
                (
                    export.function,
                    document
                        .functions
                        .get(&export.function)
                        .ok_or_else(|| "missing exported function".to_owned())
                        .and_then(|function| {
                            Lifter::new(document, captions, Unit::Library(export.function), library)
                                .item(name, function, export)
                        }),
                ),
            )),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let main = if document.exports.is_empty() {
        let mut lifter = Lifter::new(document, captions, Unit::Main, library);
        let block = document.main.as_deref().unwrap_or_default();
        lifter
            .block(block, &Site::new(Unit::Main, Vec::new()))
            .map(Main::Cell)
    } else {
        module_main(document, captions, library).map(Main::Module)
    };
    Lifted { functions, main }
}

fn module_main(
    document: &k::Document,
    captions: Option<&k::Captions>,
    library: &dyn lash_kernel_dialect::Library,
) -> Lift<Vec<ModulePart>> {
    let parts = lash_kernel_dialect::regions::module_parts(document)
        .ok_or_else(|| "module exports do not match their Main bindings".to_owned())?;
    let main = document.main.as_deref().unwrap_or_default();
    let mut lifter = Lifter::new(document, captions, Unit::Main, library);
    parts
        .into_iter()
        .filter_map(|part| {
            use lash_kernel_dialect::regions::ModulePart as Part;
            match part {
                Part::Export(name) => Some(Ok(ModulePart::Export(name))),
                Part::Statements(range) => Some(
                    lifter
                        .block_at(
                            &main[range.clone()],
                            &Site::new(Unit::Main, Vec::new()),
                            range.start,
                        )
                        .map(ModulePart::Statements),
                ),
                Part::Implicit => None,
            }
        })
        .collect()
}

/// What [`Lifter::enter`] set aside.
pub(super) struct Saved(Vec<Pending>, Vec<Caption>);

/// A temporary the front end introduced, waiting to be written where it is
/// read.
struct Pending {
    name: Name,
    value: source::Expr,
    caption: Option<Caption>,
}

pub(super) struct Lifter<'a> {
    pub(super) document: &'a k::Document,
    library: &'a dyn lash_kernel_dialect::Library,
    pub(super) imports: &'a BTreeMap<lash_vocabulary::ImportSlot, lash_vocabulary::Import>,
    captions: Option<&'a k::Captions>,
    /// The temporaries lifted and not yet read, in the order they were
    /// bound: the order the source evaluated them.
    pending: Vec<Pending>,
    /// The captions of the temporaries the statement being lifted read.
    taken_captions: Vec<Caption>,
    /// The names some statement of the unit assigns.
    assigned: BTreeSet<Name>,
    /// Original names retained in lowered function tokens, keyed by binding identity.
    pub(super) bindings: BTreeMap<Name, String>,
}

impl<'a> Lifter<'a> {
    fn new(
        document: &'a k::Document,
        captions: Option<&'a k::Captions>,
        unit: Unit,
        library: &'a dyn lash_kernel_dialect::Library,
    ) -> Self {
        let imports = match &unit {
            Unit::Library(id) => document
                .functions
                .get(id)
                .and_then(|f| f.body())
                .map_or(&document.imports, |body| &body.imports),
            _ => &document.imports,
        };
        let mut assigned = BTreeSet::new();
        let body = match &unit {
            Unit::Library(id) => document
                .functions
                .get(id)
                .and_then(|f| f.body())
                .map(|body| body.block.as_slice()),
            _ => document.main.as_deref(),
        };
        if let Some(body) = body {
            assigned_in(body, &mut assigned);
        }
        let mut bindings = BTreeMap::new();
        if let Some(body) = body {
            let mut pending = vec![k::Node::Block(body)];
            while let Some(node) = pending.pop() {
                let binding = match node {
                    k::Node::Stmt(k::Stmt::Let {
                        name,
                        value: k::Rhs::Expr(value),
                    })
                    | k::Node::Stmt(k::Stmt::Assign {
                        place: k::Place::Variable(name),
                        value: k::Rhs::Expr(value),
                        ..
                    }) => Some((name, value)),
                    _ => None,
                };
                if let Some((name, k::Expr::Tuple(parts))) = binding
                    && let [
                        k::Expr::Literal(k::Literal::Text(tag)),
                        _,
                        k::Expr::Literal(k::Literal::Text(original)),
                        _,
                        k::Expr::Closure(_),
                        _,
                    ] = parts.as_slice()
                    && tag == "ts.function"
                    && source::identifier(original)
                    && name.as_str().strip_prefix(original).is_some_and(|suffix| {
                        suffix.strip_prefix('_').is_some_and(|number| {
                            !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
                        })
                    })
                {
                    bindings.insert(name.clone(), original.clone());
                }
                pending.extend(node.children());
            }
        }
        Self {
            document,
            library,
            imports,
            captions,
            pending: Vec::new(),
            taken_captions: Vec::new(),
            assigned,
            bindings,
        }
    }

    pub(super) fn library<R: Reference>(&self, id: &R) -> Option<String> {
        id.name(self.document, self.imports, self.library)
    }
    pub(super) fn is<R: Reference>(&self, id: &R, name: &str) -> bool {
        let Ok(name) = lash_vocabulary::FunctionName::new(name) else {
            return false;
        };
        let qualified = lash_kernel_dialect::authoring::helper_name(&name);
        self.library(id)
            .is_some_and(|found| found == name.as_str() || found == qualified.to_string())
    }

    /// Match the import contract authored for the library's selected helper.
    /// Display labels do not establish a helper's identity or convention.
    pub(super) fn helper_import(&self, slot: &lash_vocabulary::ImportSlot, name: &str) -> bool {
        let Some(identity) = self.library.resolve(name) else {
            return false;
        };
        let Some(header) = self.library.header(&identity) else {
            return false;
        };
        !header.has_native()
            && self.imports.get(slot).is_some_and(|import| {
                import.name == lash_kernel_dialect::authoring::helper_name(&header.name)
                    && matches!(&import.kind,
                        lash_vocabulary::ImportKind::Code { signature, convention }
                            if *signature == header.signature
                                && *convention == lash_vocabulary::Convention::Direct)
            })
    }

    fn caption(&self, target: impl Into<CaptionTarget>) -> Option<Caption> {
        self.captions?.get(&target.into()).cloned()
    }

    pub(super) fn site_caption(&self, site: &Site) -> Option<Caption> {
        self.captions?.at(site).cloned()
    }

    pub(super) fn in_function(&self, function: lash_vocabulary::FunctionId) -> Self {
        let mut inner = Self::new(
            self.document,
            self.captions,
            Unit::Library(function),
            self.library,
        );
        inner.bindings.extend(self.bindings.clone());
        inner
    }

    /// Whether some statement of the unit assigns `name` after binding it.
    pub(super) fn reassigned(&self, name: &str) -> bool {
        self.assigned.contains(&Name::new(name))
    }

    /// Where the temporary `name` stands among those waiting to be read.
    pub(super) fn pending_at(&self, name: &Name) -> Option<usize> {
        self.pending
            .iter()
            .position(|pending| pending.name == *name)
    }

    /// The value of the temporary `name`, written where it is read.
    pub(super) fn take_pending(&mut self, name: &Name) -> Option<source::Expr> {
        let at = self.pending_at(name)?;
        let pending = self.pending.remove(at);
        self.taken_captions.extend(pending.caption);
        Some(pending.value)
    }

    fn item(
        &mut self,
        name: &Name,
        function: &k::FunctionDefinition,
        entry: &k::FunctionExport,
    ) -> Lift<Item> {
        // An export's convention belongs to the document, while this lifter's
        // ordinary references belong to the defining function's body imports.
        let export_scope = Lifter::new(self.document, self.captions, Unit::Main, self.library);
        if !matches!(&entry.convention, k::StartConvention::TsDirect { await_fn }
            if export_scope.is(await_fn, "ts.await"))
        {
            return refuse("an entry the dialect starts directly");
        }
        if !source::identifier(name.as_str()) {
            return refuse("an entry name TypeScript cannot spell");
        }
        if entry.signature.params.len() != function.signature.params.len()
            || entry
                .signature
                .params
                .iter()
                .zip(function.signature.params.iter().map(|param| &param.name))
                .any(|(param, name)| param.name != *name)
        {
            return refuse("an entry's parameters are its signature's");
        }
        let mut params = Vec::with_capacity(function.signature.params.len());
        for param in &entry.signature.params {
            if !source::identifier(param.name.as_str()) {
                return refuse("a parameter name TypeScript cannot spell");
            }
            let root = FieldRoot::Param(param.name.clone());
            let ty = self.annotation(name, &param.ty, &root, &mut Vec::new())?;
            params.push(Param {
                caption: self.caption(k::ParamTarget {
                    function: name.clone(),
                    param: param.name.clone(),
                }),
                name: param.name.as_str().to_owned(),
                optional: false,
                ty,
            });
        }
        let result = self.annotation(
            name,
            &entry.signature.result,
            &FieldRoot::Result,
            &mut Vec::new(),
        )?;
        let site = Site::new(Unit::Library(entry.function), Vec::new());
        let mut statements: &[k::Stmt] = &function
            .body()
            .ok_or_else(|| "native export has no authored body".to_owned())?
            .block;
        // The implicit end of an async function returns undefined.
        if let [rest @ .., k::Stmt::ReturnVia { value, policy }] = statements
            && matches!(value, k::Expr::Literal(k::Literal::Absent))
            && self.is(policy, "ts.promise.adopt")
            && self
                .captions
                .is_none_or(|captions| captions.at(&site.child(rest.len() as u32)).is_none())
        {
            statements = rest;
        }
        let body = self.block(statements, &site)?;
        Ok(Item {
            is_async: entry.asynchronous,
            caption: self.caption(site),
            name: name.as_str().to_owned(),
            params,
            result,
            body,
        })
    }

    /// The annotation `ty` is written with, `None` for `Any`, its record
    /// fields captioned under `path`.
    fn annotation(
        &self,
        function: &Name,
        ty: &lash_vocabulary::Type,
        root: &FieldRoot,
        path: &mut Vec<String>,
    ) -> Lift<Option<Type>> {
        Ok(match ty {
            lash_vocabulary::Type::Any => None,
            _ => Some(self.ts_type(Some(function), ty, root, path)?),
        })
    }

    pub(super) fn ts_type(
        &self,
        function: Option<&Name>,
        ty: &lash_vocabulary::Type,
        root: &FieldRoot,
        path: &mut Vec<String>,
    ) -> Lift<Type> {
        Ok(match ty {
            lash_vocabulary::Type::Json => Type::Word("JsonValue"),
            lash_vocabulary::Type::Enum(values) => {
                let mut values: Vec<_> = values.iter().cloned().map(Type::Literal).collect();
                if values.len() == 1 {
                    values.remove(0)
                } else {
                    Type::Union(values)
                }
            }
            lash_vocabulary::Type::Any => return refuse("an unconstrained nested kernel type"),
            lash_vocabulary::Type::Text => Type::Word("string"),
            lash_vocabulary::Type::Number => Type::Word("number"),
            lash_vocabulary::Type::Bool => Type::Word("boolean"),
            lash_vocabulary::Type::Null => Type::Word("null"),
            lash_vocabulary::Type::List(item) => {
                Type::Array(Box::new(self.ts_type(function, item, root, path)?))
            }
            lash_vocabulary::Type::Union(members) => {
                let mut written = Vec::new();
                let mut index = 0;
                while index < members.len() {
                    match &members[index..] {
                        [
                            lash_vocabulary::Type::Null,
                            lash_vocabulary::Type::Absent,
                            ..,
                        ] => {
                            written.push(Type::Word("undefined"));
                            index += 2;
                        }
                        [
                            lash_vocabulary::Type::Any
                            | lash_vocabulary::Type::Absent
                            | lash_vocabulary::Type::Union(_),
                            ..,
                        ] => {
                            return refuse("a union member no annotation writes");
                        }
                        [member, ..] => {
                            written.push(self.ts_type(function, member, root, path)?);
                            index += 1;
                        }
                        [] => unreachable!("the loop stops at the end"),
                    }
                }
                Type::Union(written)
            }
            lash_vocabulary::Type::Record(record) => {
                let mut properties = Vec::with_capacity(record.fields.len());
                for field in &record.fields {
                    path.push(field.name.clone());
                    let caption = function.and_then(|function| {
                        self.caption(k::FieldTarget {
                            function: function.clone(),
                            path: FieldPath {
                                root: root.clone(),
                                fields: path.clone(),
                            },
                        })
                    });
                    let ty = self.ts_type(function, &field.ty, root, path);
                    path.pop();
                    properties.push(Property {
                        caption,
                        name: field.name.clone(),
                        optional: field.optional,
                        ty: ty?,
                    });
                }
                Type::Object(
                    properties,
                    record
                        .rest
                        .as_ref()
                        .map(|rest| self.ts_type(function, rest, root, path).map(Box::new))
                        .transpose()?,
                )
            }
            _ => return refuse("a type no annotation writes"),
        })
    }

    /// The statements of `block`, at `site`.
    pub(super) fn block(&mut self, block: &[k::Stmt], site: &Site) -> Lift<Vec<Stmt>> {
        self.block_at(block, site, 0)
    }

    /// The temporaries waiting to be read, set aside while a function value
    /// nested in the statement being lifted is: it reads none of them.
    pub(super) fn enter(&mut self) -> Saved {
        Saved(
            std::mem::take(&mut self.pending),
            std::mem::take(&mut self.taken_captions),
        )
    }

    pub(super) fn leave(&mut self, saved: Saved) {
        self.pending = saved.0;
        self.taken_captions = saved.1;
    }

    /// [`Self::block`] of statements that begin at child `first` of the
    /// block at `site`.
    pub(super) fn block_at(
        &mut self,
        block: &[k::Stmt],
        site: &Site,
        first: usize,
    ) -> Lift<Vec<Stmt>> {
        let outer = self.pending.len();
        let mut out = Vec::new();
        let mut index = 0;
        let mut predeclared = BTreeSet::new();
        while index < block.len() {
            let here = site.child((first + index) as u32);
            let caption = self
                .captions
                .and_then(|captions| captions.at(&here))
                .cloned();
            let ladder = match self.method_ladder(&block[index..], site, first + index)? {
                Some(ladder) => Some(ladder),
                None => self.read_ladder(&block[index..])?,
            };
            if let Some((name, value, taken)) = ladder {
                for later in 1..taken {
                    if self.captions.is_some_and(|captions| {
                        captions
                            .at(&site.child((first + index + later) as u32))
                            .is_some()
                    }) {
                        return refuse("a caption inside a lowered read");
                    }
                }
                self.pending.push(Pending {
                    name,
                    value,
                    caption,
                });
                index += taken;
                continue;
            }
            if let k::Stmt::Let {
                name,
                value: k::Rhs::Expr(value),
            } = &block[index]
                && temporary(name)
            {
                let value = self.expr(value)?;
                self.pending.push(Pending {
                    name: name.clone(),
                    value,
                    caption,
                });
                index += 1;
                continue;
            }
            // The TDZ cell is emitted before a closure captures a declaration
            // that appears later. Its source is that later declaration.
            if let k::Stmt::Let {
                name,
                value: k::Rhs::Expr(k::Expr::Tuple(items)),
            } = &block[index]
                && items.is_empty()
                && self.reassigned(name.as_str())
            {
                predeclared.insert(name.clone());
                index += 1;
                continue;
            }
            let kind = self.statement(&block[index], &here)?;
            let kind = match (&block[index], kind) {
                (
                    k::Stmt::Assign {
                        place: k::Place::Variable(name),
                        ..
                    },
                    source::StmtKind::Assign(_, "=", value),
                ) if predeclared.remove(name) => {
                    source::StmtKind::Let(self.binding(name)?, Some(value))
                }
                (_, kind) => kind,
            };
            // A temporary's caption names the statement that reads it.
            let mut caption = caption;
            for pending in std::mem::take(&mut self.taken_captions) {
                if caption.replace(pending).is_some() {
                    return refuse("two captions on one written statement");
                }
            }
            out.push(Stmt { caption, kind });
            index += 1;
        }
        if self.pending.len() > outer {
            return refuse("a temporary nothing reads");
        }
        Ok(out)
    }
}

/// A name the front end makes for a value it evaluates ahead of its use.
pub(super) fn temporary(name: &Name) -> bool {
    name.as_str()
        .strip_prefix('t')
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

fn assigned_in(block: &[k::Stmt], out: &mut BTreeSet<Name>) {
    for statement in block {
        match statement {
            k::Stmt::Assign {
                place: k::Place::Variable(name),
                ..
            } => {
                out.insert(name.clone());
            }
            k::Stmt::If {
                then_block,
                else_block,
                ..
            } => {
                assigned_in(then_block, out);
                assigned_in(else_block, out);
            }
            k::Stmt::For {
                body, else_block, ..
            }
            | k::Stmt::While {
                body, else_block, ..
            } => {
                assigned_in(body, out);
                assigned_in(else_block, out);
            }
            k::Stmt::Try(scope) => {
                assigned_in(&scope.body, out);
                if let Some(catch) = &scope.catch {
                    assigned_in(&catch.body, out);
                }
                if let Some(finally) = &scope.finally {
                    assigned_in(finally, out);
                }
            }
            _ => {}
        }
    }
}

pub(super) trait Reference {
    fn name(
        &self,
        document: &k::Document,
        imports: &BTreeMap<lash_vocabulary::ImportSlot, lash_vocabulary::Import>,
        library: &dyn lash_kernel_dialect::Library,
    ) -> Option<String>;
}
impl Reference for lash_vocabulary::FunctionId {
    fn name(
        &self,
        document: &k::Document,
        _imports: &BTreeMap<lash_vocabulary::ImportSlot, lash_vocabulary::Import>,
        library: &dyn lash_kernel_dialect::Library,
    ) -> Option<String> {
        document
            .functions
            .get(self)
            .map(|function| function.name.to_string())
            .or_else(|| {
                document
                    .requires
                    .natives
                    .contains(self)
                    .then(|| library.header(self).map(|header| header.name.to_string()))
                    .flatten()
            })
    }
}
impl Reference for lash_vocabulary::ImportSlot {
    fn name(
        &self,
        _document: &k::Document,
        imports: &BTreeMap<lash_vocabulary::ImportSlot, lash_vocabulary::Import>,
        library: &dyn lash_kernel_dialect::Library,
    ) -> Option<String> {
        let name = &imports.get(self)?.name;
        Some(
            library
                .imported(name)
                .map_or_else(|| name.to_string(), |source| source.to_string()),
        )
    }
}
impl Reference for k::FunctionTarget {
    fn name(
        &self,
        document: &k::Document,
        imports: &BTreeMap<lash_vocabulary::ImportSlot, lash_vocabulary::Import>,
        library: &dyn lash_kernel_dialect::Library,
    ) -> Option<String> {
        match self {
            Self::Local(id) => id.name(document, imports, library),
            Self::Import(slot) => slot.name(document, imports, library),
        }
    }
}
