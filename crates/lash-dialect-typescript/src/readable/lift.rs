//! Lifting: a kernel region back to the TypeScript a model would have
//! written for it. Each rule inverts one shape the front end lowers to
//! (`crate::lower`); a node no rule recognizes leaves its region unlifted.
//! Lifting only proposes: [`super::print`] keeps a region's lift only when
//! reading it back gives the region again.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{self as k, Caption, CaptionTarget, FieldPath, FieldRoot};

use lash_kernel_value::Name;
use {lash_kernel_value::sites::Site, lash_kernel_value::sites::Unit};

use super::source::{self, Item, Param, Property, Stmt, Type};

/// Why a region did not lift.
pub(super) type Lift<T> = Result<T, String>;

/// How a lift spells what it may spell more than one way. [`super::print`]
/// tries the plainest style first and keeps the first one that reads back.
#[derive(Clone, Copy)]
pub(super) struct Style {
    /// Captured module functions retain their `const` declaration form.
    pub(super) const_functions: bool,
    /// Front-end temporaries keep their own `const`s instead of being written
    /// where they are read.
    pub(super) preserve_temporaries: bool,
    /// Numeric natives over checked `number` operands are written as the
    /// operators they lower from (`n < 1`), not as `num.lt(num.checked_float(n), 1)`.
    pub(super) operators: bool,
}

pub(super) fn refuse<T>(why: impl Into<String>) -> Lift<T> {
    Err(why.into())
}

/// What a document lifts to: each declared function on its own, and
/// `main` as the order of the module's entries or as a cell's statements.
pub(super) struct Lifted {
    pub(super) functions: BTreeMap<Name, (lash_kernel_value::FunctionId, Lift<Item>)>,
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
    Alias { local: Name, exported: Name },
    Statements(Vec<Stmt>),
}

pub(super) fn document(
    document: &k::Document,
    captions: Option<&k::Captions>,
    library: &dyn lash_kernel_dialect::Library,
    style: Style,
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
                            Lifter::new(
                                document,
                                captions,
                                Unit::Library(export.function),
                                library,
                                style,
                            )
                            .item(name, function, export)
                        }),
                ),
            )),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let main = if document.exports.is_empty() {
        let mut lifter = Lifter::new(document, captions, Unit::Main, library, style);
        let block = document.main.as_deref().unwrap_or_default();
        lifter
            .block(block, &Site::new(Unit::Main, Vec::new()))
            .map(Main::Cell)
    } else {
        module_main(document, captions, library, style).map(Main::Module)
    };
    Lifted { functions, main }
}

fn module_main(
    document: &k::Document,
    captions: Option<&k::Captions>,
    library: &dyn lash_kernel_dialect::Library,
    style: Style,
) -> Lift<Vec<ModulePart>> {
    let parts = lash_kernel_dialect::regions::module_parts(document)
        .ok_or_else(|| "module exports do not match their Main bindings".to_owned())?;
    let main = document.main.as_deref().unwrap_or_default();
    let mut lifter = Lifter::new(document, captions, Unit::Main, library, style);
    parts
        .into_iter()
        .filter_map(|part| {
            use lash_kernel_dialect::regions::ModulePart as Part;
            match part {
                Part::Export(name) => Some(Ok(ModulePart::Export(name))),
                Part::Alias { local, exported } => Some(Ok(ModulePart::Alias { local, exported })),
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
    pub(super) imports: &'a BTreeMap<lash_kernel_value::ImportSlot, lash_kernel_value::Import>,
    captions: Option<&'a k::Captions>,
    /// The temporaries lifted and not yet read, in the order they were
    /// bound: the order the source evaluated them.
    pending: Vec<Pending>,
    pub(super) style: Style,
    /// The captions of the temporaries the statement being lifted read.
    taken_captions: Vec<Caption>,
    /// The names some statement of the unit assigns.
    assigned: BTreeSet<Name>,
    /// Original names retained in lowered function tokens, keyed by binding identity.
    pub(super) bindings: BTreeMap<Name, String>,
    pub(super) module_helpers: BTreeSet<Name>,
    /// Bindings consumed by the conditional process-handle await adapter.
    pub(super) process_awaits: BTreeSet<Name>,
}

impl<'a> Lifter<'a> {
    pub(super) fn new(
        document: &'a k::Document,
        captions: Option<&'a k::Captions>,
        unit: Unit,
        library: &'a dyn lash_kernel_dialect::Library,
        style: Style,
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
        for definition in document.functions.values() {
            if let Some(body) = definition.body() {
                assigned_in(&body.block, &mut assigned);
            }
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
                {
                    bindings.insert(name.clone(), original.clone());
                }
                pending.extend(node.children());
            }
        }
        let mut lifter = Self {
            document,
            library,
            imports,
            captions,
            pending: Vec::new(),
            style,
            taken_captions: Vec::new(),
            assigned,
            bindings,
            module_helpers: BTreeSet::new(),
            process_awaits: BTreeSet::new(),
        };
        if let Some(body) = body {
            let mut pending = vec![k::Node::Block(body)];
            while let Some(node) = pending.pop() {
                if let k::Node::Expr(k::Expr::If { then, r#else, .. }) = node
                    && let Some(name) = lifter.process_await_name(then, r#else).cloned()
                {
                    lifter.process_awaits.insert(name);
                }
                pending.extend(node.children());
            }
        }
        lifter
    }

    pub(super) fn library<R: Reference>(&self, id: &R) -> Option<String> {
        id.name(self.document, self.imports, self.library)
    }
    pub(super) fn is<R: Reference>(&self, id: &R, name: &str) -> bool {
        let Ok(name) = lash_kernel_value::FunctionName::new(name) else {
            return false;
        };
        self.library(id).is_some_and(|found| found == name.as_str())
    }

    fn caption(&self, target: impl Into<CaptionTarget>) -> Option<Caption> {
        self.captions?.get(&target.into()).cloned()
    }

    pub(super) fn site_caption(&self, site: &Site) -> Option<Caption> {
        self.captions?.at(site).cloned()
    }

    pub(super) fn in_function(&self, function: lash_kernel_value::FunctionId) -> Self {
        let mut inner = Self::new(
            self.document,
            self.captions,
            Unit::Library(function),
            self.library,
            self.style,
        );
        inner.bindings.extend(self.bindings.clone());
        inner.module_helpers.extend(self.module_helpers.clone());
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
        let export_scope = Lifter::new(
            self.document,
            self.captions,
            Unit::Main,
            self.library,
            self.style,
        );
        if !matches!(&entry.convention, k::StartConvention::TsDirect { await_fn }
            if export_scope.is(await_fn, "ts.entry_result"))
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
        let end = crate::constructs::conventions::RETURNS
            .body_end(function, |target, name| self.is(target, name))
            .map_err(|reason| reason.to_string())?;
        if let Some(end) = end
            && self
                .captions
                .is_none_or(|captions| captions.at(&site.child(end as u32)).is_none())
        {
            statements = &statements[..end];
        }
        // Entry lowering recreates reachable module helpers in its private
        // scope. Refer to their module declarations so re-lowering recreates
        // that scope, including its original closure and binding identities.
        let skip = statements
            .iter()
            .take_while(|statement| self.module_capture(statement))
            .count();
        for statement in &statements[..skip] {
            if let k::Stmt::Let { name, .. } = statement
                && self.document.exports.contains_key(name)
            {
                self.module_helpers.insert(name.clone());
            }
        }
        let body = self.block_at(&statements[skip..], &site, skip)?;
        let captured_body = self.module_body(name, &body)?;
        let const_binding = captured_body.is_some() && self.style.const_functions;
        let body = captured_body.unwrap_or(source::Body::Block(body));
        Ok(Item {
            const_binding,
            is_async: entry.asynchronous,
            caption: self.caption(site),
            name: name.as_str().to_owned(),
            params,
            result,
            body,
        })
    }

    fn module_capture(&self, statement: &k::Stmt) -> bool {
        let k::Stmt::Let {
            name,
            value: k::Rhs::Expr(value),
        } = statement
        else {
            return false;
        };
        if self.document.exports.contains_key(name) {
            super::closures::authored_name(value) == Some(name.as_str())
        } else {
            self.document
                .main
                .as_deref()
                .is_some_and(|main| main.contains(statement))
        }
    }

    /// The annotation `ty` is written with, `None` for `Any`, its record
    /// fields captioned under `path`.
    fn annotation(
        &self,
        function: &Name,
        ty: &lash_kernel_value::Type,
        root: &FieldRoot,
        path: &mut Vec<String>,
    ) -> Lift<Option<Type>> {
        Ok(match ty {
            lash_kernel_value::Type::Any => None,
            _ => Some(self.ts_type(Some(function), ty, root, path)?),
        })
    }

    pub(super) fn ts_type(
        &self,
        function: Option<&Name>,
        ty: &lash_kernel_value::Type,
        root: &FieldRoot,
        path: &mut Vec<String>,
    ) -> Lift<Type> {
        Ok(match ty {
            lash_kernel_value::Type::Json => Type::Word("JsonValue"),
            lash_kernel_value::Type::Enum(values) => {
                let mut values: Vec<_> = values.iter().cloned().map(Type::Literal).collect();
                if values.len() == 1 {
                    values.remove(0)
                } else {
                    Type::Union(values)
                }
            }
            lash_kernel_value::Type::Any => return refuse("an unconstrained nested kernel type"),
            lash_kernel_value::Type::Text => Type::Word("string"),
            lash_kernel_value::Type::Number => Type::Word("number"),
            lash_kernel_value::Type::Int => Type::Word("bigint"),
            lash_kernel_value::Type::Bool => Type::Word("boolean"),
            lash_kernel_value::Type::Null => Type::Word("null"),
            lash_kernel_value::Type::List(item) => {
                Type::Array(Box::new(self.ts_type(function, item, root, path)?))
            }
            lash_kernel_value::Type::Union(members) => {
                let mut written = Vec::new();
                let mut index = 0;
                while index < members.len() {
                    match &members[index..] {
                        [
                            lash_kernel_value::Type::Null,
                            lash_kernel_value::Type::Absent,
                            ..,
                        ] => {
                            written.push(Type::Word("undefined"));
                            index += 2;
                        }
                        [
                            lash_kernel_value::Type::Any
                            | lash_kernel_value::Type::Absent
                            | lash_kernel_value::Type::Union(_),
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
            lash_kernel_value::Type::Record(record) => {
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
                    match &record.rest {
                        lash_kernel_value::RecordRest::Closed => {
                            Some(Box::new(Type::Word("never")))
                        }
                        lash_kernel_value::RecordRest::Drop => None,
                        lash_kernel_value::RecordRest::Keep(rest) => {
                            Some(Box::new(self.ts_type(function, rest, root, path)?))
                        }
                    },
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

    /// An expression-scoped temporary is spelled at its one consuming read,
    /// just like a statement temporary. Unconsumed effects cannot disappear.
    pub(super) fn scoped_temporary(
        &mut self,
        name: &Name,
        value: &k::Expr,
        body: &k::Expr,
    ) -> Lift<source::Expr> {
        let value = self.expr(value)?;
        self.pending.push(Pending {
            name: name.clone(),
            value,
            caption: None,
        });
        let result = self.expr(body);
        if let Some(index) = self
            .pending
            .iter()
            .position(|pending| pending.name == *name)
        {
            self.pending.remove(index);
            return refuse("an unconsumed expression temporary");
        }
        result
    }

    /// How many statements from the start of `block`, child `at` of the
    /// block at `site`, become one temporary waiting to be read: a lowered
    /// read or method, a short circuit, or a front-end temporary. Zero when
    /// the first statement is written as itself.
    fn pend(&mut self, block: &[k::Stmt], site: &Site, at: usize) -> Lift<usize> {
        let captions = self.captions;
        let caption_at = |later: usize| {
            captions
                .and_then(|captions| captions.at(&site.child((at + later) as u32)))
                .cloned()
        };
        let caption = caption_at(0);
        if let k::Stmt::Assign {
            place: k::Place::Variable(name),
            value,
            ..
        } = &block[0]
        {
            let input = match value {
                k::Rhs::Expr(k::Expr::Act(input)) => Some((input.kind.clone(), input.args.clone())),
                k::Rhs::Action(action) => {
                    let input = super::statements::act_of(action);
                    Some((input.kind, input.args))
                }
                _ => None,
            };
            if let Some((k::ActKind::Call(k::Callee::Local(id)), args)) = input
                && crate::constructs::numbers::BIGINT_INPUT
                    .call(&id, &args, &|id, helper| self.is(id, helper))
                    .is_some_and(|value| matches!(value, k::Expr::Variable(value) if value == name))
            {
                return Ok(1);
            }
        }
        if let k::Stmt::Let { name, value } = &block[0]
            && self.process_awaits.contains(name)
            && match value {
                k::Rhs::Expr(value) => self.process_await_binding(name, value),
                k::Rhs::Action(k::Action::Await { policy, .. }) => self.is(policy, "ts.await"),
                _ => false,
            }
        {
            let value = self.rhs(value)?;
            self.pending.push(Pending {
                name: name.clone(),
                value,
                caption,
            });
            return Ok(1);
        }
        // A primitive text method's receiver check, and its slice bounds:
        // the method call they precede writes and lowers them again.
        if self.receiver_check(&block[0]) {
            self.taken_captions.extend(caption);
            return Ok(1);
        }
        if let Some(bounds) = self.slice_bounds(block) {
            let captions = [caption, caption_at(1)];
            self.taken_captions.extend(captions.into_iter().flatten());
            for (name, value) in bounds {
                self.pending.push(Pending {
                    name,
                    value,
                    caption: None,
                });
            }
            return Ok(2);
        }
        if let Some((name, value)) = self.ends_with(block)? {
            let captions = [caption, caption_at(1), caption_at(2)];
            self.taken_captions.extend(captions.into_iter().flatten());
            self.pending.push(Pending {
                name,
                value,
                caption: None,
            });
            return Ok(3);
        }
        if let Some((name, value)) = self.short_circuit(block, site, at)? {
            self.pending.push(Pending {
                name,
                value,
                caption,
            });
            return Ok(2);
        }
        let ladder = match self.array_ladder(block, site, at)? {
            Some(ladder) => Some(ladder),
            None => self.read_ladder(block)?,
        };
        if let Some((name, value, taken)) = ladder {
            if (1..taken).any(|later| caption_at(later).is_some()) {
                return refuse("a caption inside a lowered read");
            }
            self.pending.push(Pending {
                name,
                value,
                caption,
            });
            return Ok(taken);
        }
        // A helper call's result is a temporary too: reading the call where
        // it is read hoists it again.
        if let k::Stmt::Let { name, value } = &block[0]
            && matches!(
                value,
                k::Rhs::Expr(_) | k::Rhs::Action(k::Action::Call { .. })
            )
            && temporary(name)
            && !self.style.preserve_temporaries
        {
            let value = self.rhs(value)?;
            self.pending.push(Pending {
                name: name.clone(),
                value,
                caption,
            });
            return Ok(1);
        }
        Ok(0)
    }

    /// `a || b` (`a && b`, `a ?? b`) whose right operand needs statements of its own:
    /// `let t = a; if t {} else { …; set t = b }`, the arm's statements
    /// all temporaries `b` reads.
    fn short_circuit(
        &mut self,
        block: &[k::Stmt],
        site: &Site,
        at: usize,
    ) -> Lift<Option<(Name, source::Expr)>> {
        let [
            k::Stmt::Let { name, value: left },
            k::Stmt::If {
                condition,
                then_block,
                else_block,
            },
            ..,
        ] = block
        else {
            return Ok(None);
        };
        let tested = match condition {
            k::Expr::Call { function, args } if self.is(function, "ts.to_boolean") => {
                args.as_slice()
            }
            condition => std::slice::from_ref(condition),
        };
        if !temporary(name) || self.style.preserve_temporaries {
            return Ok(None);
        }
        let (operator, arm, child) = if self.nullish(condition, name) && else_block.is_empty() {
            ("??", then_block, 1)
        } else if tested == [k::Expr::Variable(name.clone())] {
            match (then_block.is_empty(), else_block.is_empty()) {
                (true, false) => ("||", else_block, 2),
                (false, true) => ("&&", then_block, 1),
                _ => return Ok(None),
            }
        } else {
            return Ok(None);
        };
        let [
            prelude @ ..,
            k::Stmt::Assign {
                place: k::Place::Variable(set),
                value: right,
                order: k::AssignOrder::ValueFirst,
            },
        ] = arm.as_slice()
        else {
            return Ok(None);
        };
        if set != name {
            return Ok(None);
        }
        let left = self.rhs(left)?;
        let outer = self.pending.len();
        let arm_site = site.child((at + 1) as u32).child(child);
        let mut index = 0;
        while index < prelude.len() {
            match self.pend(&prelude[index..], &arm_site, index)? {
                0 => return refuse("a statement in a short circuit's right operand"),
                taken => index += taken,
            }
        }
        let right = self.rhs(right)?;
        if self.pending.len() > outer {
            return refuse("a temporary a short circuit's right operand does not read");
        }
        Ok(Some((
            name.clone(),
            source::Expr::Binary(operator, Box::new(left), Box::new(right)),
        )))
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
            let pended = self.pend(&block[index..], site, first + index)?;
            if pended > 0 {
                index += pended;
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
        imports: &BTreeMap<lash_kernel_value::ImportSlot, lash_kernel_value::Import>,
        library: &dyn lash_kernel_dialect::Library,
    ) -> Option<String>;
}
impl Reference for lash_kernel_value::FunctionId {
    fn name(
        &self,
        document: &k::Document,
        _imports: &BTreeMap<lash_kernel_value::ImportSlot, lash_kernel_value::Import>,
        library: &dyn lash_kernel_dialect::Library,
    ) -> Option<String> {
        (document.library.contains(self) || document.requires.natives.contains(self))
            .then(|| {
                let header = library.header(self)?;
                (library.resolve(header.name.as_str()) == Some(*self))
                    .then(|| header.name.to_string())
            })
            .flatten()
    }
}
impl Reference for k::FunctionTarget {
    fn name(
        &self,
        document: &k::Document,
        imports: &BTreeMap<lash_kernel_value::ImportSlot, lash_kernel_value::Import>,
        library: &dyn lash_kernel_dialect::Library,
    ) -> Option<String> {
        match self {
            Self::Local(id) => id.name(document, imports, library),
            Self::Import(_) => None,
        }
    }
}
