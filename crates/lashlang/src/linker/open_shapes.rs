//! Which object shapes the field guard may trust (FIG-3626).
//!
//! The linker refuses a read or write of a field an object's type lacks. That
//! is sound only while the type is the whole truth about the object: a
//! statically closed shape. In JavaScript any other object answers a missing
//! field with `undefined`, so the guard must not fire on one. An object stops
//! being closed, for the whole program, when:
//!
//! - it is written through a computed key (`o[k] = v`, which is also how a
//!   spread or a computed key in an object literal is lowered);
//! - it escapes: its reference reaches code that is not a plain field read
//!   (a call argument, a container, a return value), where that code may add
//!   fields the linker never sees.
//!
//! Binding a place to another name (`const alias = o`, the pinned base every
//! member write lowers through, a destructured field) is not an escape: the
//! alias is tracked, and whatever opens the alias opens the place it names.
//!
//! The analysis is flow-insensitive, like the ruling it implements: a binding
//! whose object escapes or takes a computed write anywhere in the program is
//! open everywhere, including before the trigger and on the first pass of a
//! loop whose later iterations run after it. It is conservative: an escape
//! opens everything the escaping value reaches, and an element read (`o[k]`)
//! stands for every field of `o`.

use super::*;

/// Stdlib methods whose arguments neither survive the call nor reach guest
/// code: the result is built from the argument, never shares it.
const NON_RETAINING_STDLIB: &[&str] = &["__consoleObservationText", "Object.keys", "Array.isArray"];

/// An alias chain can make an opened path grow without end (`t = o.a` and
/// `o = t`); past this depth the path is cut, which opens a larger subtree.
const MAX_OPEN_PATH: usize = 8;

/// One step of a path below a binding: a named field, or `None` for an
/// element read through a computed key, which may be any field or item.
type Step = Option<AstString>;

/// The places whose object shapes are open, per root binding name. An empty
/// path opens the binding's whole value.
#[derive(Debug, Default)]
pub(super) struct OpenPlaces {
    by_root: BTreeMap<String, Vec<Vec<Step>>>,
    /// Bindings assigned a place, with the place: `alias = root.path`.
    aliases: Vec<(String, String, Vec<Step>)>,
}

impl OpenPlaces {
    pub(super) fn of(program: &Program) -> Self {
        let mut open = Self::default();
        let mut work = vec![(&program.main, true)];
        for declaration in &program.declarations {
            match declaration {
                Declaration::Process(process) => work.push((&process.body, true)),
                Declaration::Function(function) => work.push((&function.body, true)),
                Declaration::Type(_) => {}
            }
        }
        // A work list rather than recursion: the program may nest as deep as
        // the parser admits, and this walk must not be what overflows.
        while let Some((expr, escapes)) = work.pop() {
            open.visit(expr, escapes, &mut work);
        }
        open.propagate_through_aliases();
        open
    }

    /// Opens, below each aliased place, every path its alias opens, until
    /// nothing changes.
    fn propagate_through_aliases(&mut self) {
        let aliases = std::mem::take(&mut self.aliases);
        loop {
            let mut changed = false;
            for (alias, root, path) in &aliases {
                for opened in self.paths(alias).unwrap_or_default().to_vec() {
                    let mut target = path.clone();
                    target.extend(opened);
                    changed |= self.open(root, target);
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// Visits `expr`, whose value escapes when `escapes` holds, pushing its
    /// children with the context each one's value is used in.
    fn visit<'e>(&mut self, expr: &'e Expr, escapes: bool, work: &mut Vec<(&'e Expr, bool)>) {
        if let Some((root, path, keys)) = place(expr) {
            if escapes {
                self.open(root.as_str(), path);
            }
            work.extend(keys.into_iter().map(|key| (key, false)));
            return;
        }
        match expr {
            // A read of a value's field or element keeps no reference to the
            // value itself.
            Expr::Field { target, .. } => work.push((&**target, false)),
            Expr::Index { target, index } => work.extend([(&**target, false), (&**index, false)]),
            Expr::Block(items) => {
                let last = items.len().saturating_sub(1);
                work.extend(
                    items
                        .iter()
                        .enumerate()
                        .map(|(index, item)| (item, escapes && index == last)),
                );
            }
            Expr::LabelAnnotated { expr, .. } | Expr::Role { expr, .. } => {
                work.push((&**expr, escapes));
            }
            Expr::If {
                condition,
                then_block,
                else_block,
            } => work.extend([
                (&**condition, false),
                (&**then_block, escapes),
                (&**else_block, escapes),
            ]),
            // `&&`, `||` and `??` answer one of their operands.
            Expr::JavaScriptLogical { left, right, .. } => {
                work.extend([(&**left, escapes), (&**right, escapes)]);
            }
            Expr::Binary { left, op, right } => {
                let operands_escape = match op {
                    crate::ast::BinaryOp::And | crate::ast::BinaryOp::Or => escapes,
                    // List concatenation shares the operands' elements.
                    crate::ast::BinaryOp::Add => true,
                    _ => false,
                };
                work.extend([(&**left, operands_escape), (&**right, operands_escape)]);
            }
            Expr::JavaScriptBinary { left, right, .. } => {
                work.extend([(&**left, false), (&**right, false)]);
            }
            Expr::Unary { expr, .. } | Expr::JavaScriptUnary { expr, .. } | Expr::Print(expr) => {
                work.push((&**expr, false));
            }
            Expr::While { condition, body } => {
                work.extend([(&**condition, false), (&**body, false)]);
            }
            Expr::For {
                iterable,
                bind,
                body,
                ..
            } => {
                work.push((&**iterable, true));
                work.extend(bind.iter().map(|bind| (&**bind, true)));
                work.push((&**body, false));
            }
            Expr::Assign { target, expr } => self.visit_assign(target, expr, work),
            Expr::BuiltinCall { name, args } => self.visit_builtin(name, args, work),
            _ => work.extend(expr.children().map(|child| (child, true))),
        }
    }

    fn visit_assign<'e>(
        &mut self,
        target: &'e crate::ast::AssignTarget,
        expr: &'e Expr,
        work: &mut Vec<(&'e Expr, bool)>,
    ) {
        let mut path = Vec::with_capacity(target.steps.len());
        for step in &target.steps {
            path.push(match step {
                AssignPathStep::Field(field) => Some(field.clone()),
                AssignPathStep::Index(key) => {
                    work.push((key, false));
                    None
                }
            });
        }
        // A write through a computed key opens the container it writes into.
        if let Some(None) = path.last() {
            path.pop();
            self.open(target.root.as_str(), path);
        }
        match (target.steps.is_empty(), aliased_place(expr)) {
            (true, Some((root, path, keys))) => {
                self.aliases
                    .push((target.root.to_string(), root.to_string(), path));
                work.extend(keys.into_iter().map(|key| (key, false)));
            }
            _ => work.push((expr, true)),
        }
    }

    fn visit_builtin<'e>(
        &mut self,
        name: &str,
        args: &'e [Expr],
        work: &mut Vec<(&'e Expr, bool)>,
    ) {
        let escapes = match (name, args.first()) {
            ("__typescript_stdlib", Some(Expr::String(method))) => {
                !NON_RETAINING_STDLIB.contains(&method.as_str())
            }
            // `globalThis.name` reads, writes or deletes the session slot a
            // top-level binding of that name also holds, so a read shares the
            // binding's object and a write or delete replaces it unseen.
            (
                "__typescript_global_get"
                | "__typescript_global_set"
                | "__typescript_global_delete",
                Some(Expr::String(slot)),
            ) => {
                self.open(slot.as_str(), Vec::new());
                true
            }
            _ => true,
        };
        work.extend(args.iter().map(|arg| (arg, escapes)));
    }

    /// Opens `path` below `root`; whether it was not open already.
    fn open(&mut self, root: &str, mut path: Vec<Step>) -> bool {
        path.truncate(MAX_OPEN_PATH);
        let paths = self.by_root.entry(root.to_string()).or_default();
        if paths.contains(&path) {
            return false;
        }
        paths.push(path);
        true
    }

    fn paths(&self, root: &str) -> Option<&[Vec<Step>]> {
        self.by_root.get(root).map(Vec::as_slice)
    }
}

/// A place: a binding, or a field or element path below one. Returns its root,
/// its path, and the element keys it evaluates.
fn place(expr: &Expr) -> Option<(&AstString, Vec<Step>, Vec<&Expr>)> {
    let mut steps = Vec::new();
    let mut keys = Vec::new();
    let mut current = expr;
    let root = loop {
        match current {
            Expr::Variable(name) => break name,
            Expr::Field { target, field } => {
                steps.push(Some(field.clone()));
                current = target;
            }
            Expr::Index { target, index } => {
                steps.push(None);
                keys.push(&**index);
                current = target;
            }
            _ => return None,
        }
    };
    steps.reverse();
    Some((root, steps, keys))
}

/// The place a binding is assigned, seen through the wrappers that do not
/// change its value.
fn aliased_place(expr: &Expr) -> Option<(&AstString, Vec<Step>, Vec<&Expr>)> {
    match expr {
        Expr::LabelAnnotated { expr, .. } | Expr::Role { expr, .. } => aliased_place(expr),
        _ => place(expr),
    }
}

impl Linker<'_> {
    /// `binding` as a read of `name` sees it: every object shape the program
    /// opens below that name is open.
    pub(super) fn open_binding(&self, name: &str, binding: Binding) -> Binding {
        match binding {
            Binding::Value(ty) => Binding::Value(self.open_type(name, ty)),
            other => other,
        }
    }

    pub(super) fn open_type(&self, name: &str, ty: TypeExpr) -> TypeExpr {
        let Some(paths) = self.open_places.paths(name) else {
            return ty;
        };
        paths
            .iter()
            .fold(ty, |ty, path| self.open_at(ty, path, &mut BTreeSet::new()))
    }

    /// Before a field write updates `root`'s type in `scope`, gives the scope
    /// the opened type the write's own checks saw.
    pub(super) fn open_scope_binding(&self, root: &AstString, scope: &mut Scope) {
        if let Some(binding) = scope.get(root) {
            let opened = self.open_binding(root.as_str(), binding.clone());
            if opened != binding {
                scope.bind(root.as_str(), opened);
            }
        }
    }

    fn open_at(&self, ty: TypeExpr, path: &[Step], seen: &mut BTreeSet<String>) -> TypeExpr {
        let Some((step, rest)) = path.split_first() else {
            return self.open_deep(ty, seen);
        };
        match ty {
            TypeExpr::Object(fields) => TypeExpr::Object(
                fields
                    .into_iter()
                    .map(|mut candidate| {
                        if step.as_ref().is_none_or(|field| candidate.name == *field) {
                            candidate.ty = self.open_at(candidate.ty, rest, seen);
                        }
                        candidate
                    })
                    .collect(),
            ),
            TypeExpr::List(item) if step.is_none() => {
                TypeExpr::List(Box::new(self.open_at(*item, rest, seen)))
            }
            TypeExpr::Union(items) => union_type(
                items
                    .into_iter()
                    .map(|item| self.open_at(item, path, seen))
                    .collect(),
            ),
            TypeExpr::Ref(ref name) => match self.resolve_alias(&ty, name, seen) {
                Some(resolved) => {
                    let opened = self.open_at(resolved, path, seen);
                    seen.remove(name.as_str());
                    opened
                }
                None => ty,
            },
            other => other,
        }
    }

    /// Every object shape in `ty` opened: what code holding the value may
    /// have made of it.
    fn open_deep(&self, ty: TypeExpr, seen: &mut BTreeSet<String>) -> TypeExpr {
        match ty {
            TypeExpr::Object(_) => TypeExpr::Dict,
            TypeExpr::List(item) => TypeExpr::List(Box::new(self.open_deep(*item, seen))),
            TypeExpr::Union(items) => union_type(
                items
                    .into_iter()
                    .map(|item| self.open_deep(item, seen))
                    .collect(),
            ),
            TypeExpr::Ref(ref name) => {
                if seen.contains(name.as_str()) {
                    // A recursive alias: whatever it names below here is open.
                    return TypeExpr::Any;
                }
                match self.resolve_alias(&ty, name, seen) {
                    Some(resolved) => {
                        let opened = self.open_deep(resolved, seen);
                        seen.remove(name.as_str());
                        opened
                    }
                    None => ty,
                }
            }
            other => other,
        }
    }

    /// The type an alias `name` stands for, marking it seen, or `None` for a
    /// name that is not an alias (a resource or an opaque value type) or is
    /// already being resolved.
    fn resolve_alias(
        &self,
        ty: &TypeExpr,
        name: &AstString,
        seen: &mut BTreeSet<String>,
    ) -> Option<TypeExpr> {
        let resolved = self.resolve_type_aliases(ty);
        if resolved == *ty || !seen.insert(name.to_string()) {
            return None;
        }
        Some(resolved)
    }
}
