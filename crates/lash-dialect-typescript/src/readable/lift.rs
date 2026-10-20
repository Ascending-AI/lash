//! Lifting: a kernel region back to the TypeScript a model would have
//! written for it. Each rule inverts one shape the front end lowers to
//! (`crate::lower`); a node no rule recognizes leaves its region unlifted.
//! Lifting only proposes: [`super::print`] keeps a region's lift only when
//! reading it back gives the region again.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{self as k, Caption, CaptionTarget, FieldPath, FieldRoot, Name, Site, Unit};

use super::source::{self, Item, Param, Property, Stmt, Type};

/// Why a region did not lift.
pub(super) type Lift<T> = Result<T, String>;

pub(super) fn refuse<T>(why: impl Into<String>) -> Lift<T> {
    Err(why.into())
}

/// What a document lifts to: each declared function on its own, and
/// `main` as the order of the module's entries or as a cell's statements.
pub(super) struct Lifted {
    pub(super) functions: BTreeMap<Name, Lift<Item>>,
    pub(super) main: Lift<Main>,
}

pub(super) enum Main {
    /// A module: its entries, in the order `main` binds them.
    Module(Vec<Name>),
    /// A cell: `main`'s statements.
    Cell(Vec<Stmt>),
}

pub(super) fn document(document: &k::Document, captions: Option<&k::Captions>) -> Lifted {
    let functions = document
        .functions
        .iter()
        .map(|(name, function)| {
            let mut lifter = Lifter::new(document, captions, Unit::Function(name.clone()));
            (name.clone(), lifter.item(name, function))
        })
        .collect();
    Lifted {
        functions,
        main: main(document, captions),
    }
}

fn main(document: &k::Document, captions: Option<&k::Captions>) -> Lift<Main> {
    if document.entries.is_empty() {
        if !document.functions.is_empty() {
            return refuse("a cell declares no function");
        }
        let mut lifter = Lifter::new(document, captions, Unit::Main);
        let statements = lifter.block(&document.main, &Site::new(Unit::Main, Vec::new()))?;
        return Ok(Main::Cell(statements));
    }
    let mut order = Vec::new();
    for statement in &document.main {
        let k::Stmt::Let {
            name,
            value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Function(function))),
        } = statement
        else {
            return refuse("a module's main only binds its entries");
        };
        if name != function || !document.entries.contains_key(name) || order.contains(name) {
            return refuse("a module's main binds each entry once, by its name");
        }
        order.push(name.clone());
    }
    if order.len() != document.functions.len() || order.len() != document.entries.len() {
        return refuse("every declared function is an entry main binds");
    }
    let site = Site::new(Unit::Main, Vec::new());
    if (0..document.main.len()).any(|index| {
        captions.is_some_and(|captions| captions.at(&site.child(index as u32)).is_some())
    }) {
        return refuse("main's statements carry no caption");
    }
    Ok(Main::Module(order))
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
    captions: Option<&'a k::Captions>,
    /// The temporaries lifted and not yet read, in the order they were
    /// bound: the order the source evaluated them.
    pending: Vec<Pending>,
    /// The captions of the temporaries the statement being lifted read.
    taken_captions: Vec<Caption>,
    /// The names some statement of the unit assigns.
    assigned: BTreeSet<Name>,
}

impl<'a> Lifter<'a> {
    fn new(document: &'a k::Document, captions: Option<&'a k::Captions>, unit: Unit) -> Self {
        let mut assigned = BTreeSet::new();
        let body = match &unit {
            Unit::Function(name) => document.functions.get(name).map(|f| f.body.as_slice()),
            _ => Some(document.main.as_slice()),
        };
        if let Some(body) = body {
            assigned_in(body, &mut assigned);
        }
        Self {
            document,
            captions,
            pending: Vec::new(),
            taken_captions: Vec::new(),
            assigned,
        }
    }

    /// The name of library function `id`, as the manifest lists it.
    pub(super) fn library(&self, id: &k::FunctionId) -> Option<&str> {
        self.document
            .manifest
            .functions
            .get(id)
            .map(k::FunctionName::as_str)
    }

    pub(super) fn is(&self, id: &k::FunctionId, name: &str) -> bool {
        self.library(id) == Some(name)
    }

    fn caption(&self, target: impl Into<CaptionTarget>) -> Option<Caption> {
        self.captions?.get(&target.into()).cloned()
    }

    pub(super) fn site_caption(&self, site: &Site) -> Option<Caption> {
        self.captions?.at(site).cloned()
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

    fn item(&mut self, name: &Name, function: &k::Function) -> Lift<Item> {
        let Some(entry) = self.document.entries.get(name) else {
            return refuse("a declared function that is no entry");
        };
        if !matches!(entry.convention, k::StartConvention::TsDirect { await_fn }
            if self.is(&await_fn, "ts.await"))
        {
            return refuse("an entry the dialect starts directly");
        }
        if function.start.is_some() {
            return refuse("an entry has no start adapter");
        }
        if !source::identifier(name.as_str()) {
            return refuse("an entry name TypeScript cannot spell");
        }
        if entry.signature.params.len() != function.params.len()
            || entry
                .signature
                .params
                .iter()
                .zip(&function.params)
                .any(|(param, name)| param.name != *name)
        {
            return refuse("an entry's parameters are its signature's");
        }
        let mut params = Vec::with_capacity(function.params.len());
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
        let site = Site::new(Unit::Function(name.clone()), Vec::new());
        let mut statements: &[k::Stmt] = &function.body;
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
        ty: &k::Type,
        root: &FieldRoot,
        path: &mut Vec<String>,
    ) -> Lift<Option<Type>> {
        Ok(match ty {
            k::Type::Any => None,
            _ => Some(self.ts_type(function, ty, root, path)?),
        })
    }

    fn ts_type(
        &self,
        function: &Name,
        ty: &k::Type,
        root: &FieldRoot,
        path: &mut Vec<String>,
    ) -> Lift<Type> {
        Ok(match ty {
            k::Type::Any => Type::Word("unknown"),
            k::Type::Text => Type::Word("string"),
            k::Type::Number => Type::Word("number"),
            k::Type::Bool => Type::Word("boolean"),
            k::Type::Null => Type::Word("null"),
            k::Type::List(item) => Type::Array(Box::new(self.ts_type(function, item, root, path)?)),
            k::Type::Union(members) => {
                let mut written = Vec::new();
                let mut index = 0;
                while index < members.len() {
                    match &members[index..] {
                        [k::Type::Null, k::Type::Absent, ..] => {
                            written.push(Type::Word("undefined"));
                            index += 2;
                        }
                        [k::Type::Any | k::Type::Absent | k::Type::Union(_), ..] => {
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
            k::Type::Record(record) if record.rest.is_none() => {
                let mut properties = Vec::with_capacity(record.fields.len());
                for field in &record.fields {
                    path.push(field.name.clone());
                    let caption = self.caption(k::FieldTarget {
                        function: function.clone(),
                        path: FieldPath {
                            root: root.clone(),
                            fields: path.clone(),
                        },
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
                Type::Object(properties)
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
            let kind = self.statement(&block[index], &here)?;
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
