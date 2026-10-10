//! Which `JSON.stringify(value)` calls are proven, at compile time, to be
//! given plain data (FIG-5863 spike).
//!
//! `ts.json.stringify` admits a value to its one native pass when the
//! guard `ts.json.plain` passes: a scalar, or lists and records of null,
//! bools, text and finite numbers, with no `toJSON` or `brand` field and no
//! key that begins with a digit, at most 128 values and eight container
//! levels. Where this analysis proves the guard would pass for every value
//! the argument can hold, the call is lowered to the pass itself,
//! `ts.console.json(value)`, and the guard is not run: the result is the
//! one the wrapper would give, and the charge is the guard's less.
//!
//! The proof is a static shape: the exact keys, lengths and leaf kinds of
//! the value. A shape comes from literals, from finite loop counters and
//! arithmetic on them, and from names whose value nothing can change:
//!
//! - a `const` or `let` the source declares once and never assigns, whose
//!   initialiser has a shape, or is `[]` grown only by `name.push(...)` of
//!   values of one shape;
//! - a `for (const v of name)` binding of such a list;
//! - and every use of such a name that reaches a container is one that
//!   cannot write it or let it escape: a `JSON.stringify` or `console.log`
//!   argument, a `for ... of` over it, a member read, a list's `push`.
//!
//! The analysis is by name over the whole source, like [`super::Facts`]:
//! a name any binding of which fails these is not proven anywhere.

use std::collections::{BTreeSet, HashMap, HashSet};

use lash_kernel_doc::Name;

use crate::adapter::{
    ArrayElement, AssignOp, AssignTarget, BinaryOp, CallArg, Expr, Function, FunctionBody,
    MemberProperty, ObjectProperty, OptionalOperation, Pattern, Program, PropertyKey, Stmt,
    UnaryOp, VarKind,
};

/// Whether lowering uses the proof: a switch for the spike's paired
/// measurement, on unless turned off.
static PROVING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Turns the compile-time plain-data proof on or off for later lowerings.
pub fn set_proving(on: bool) {
    PROVING.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// The most values, and the deepest container level, the guard admits.
const MAX_VALUES: usize = 128;
const MAX_DEPTH: usize = 8;
/// A number this far from zero cannot overflow under the arithmetic the
/// analysis follows.
const FINITE: f64 = 1e300;

#[derive(Clone, Debug, PartialEq)]
enum Shape {
    Null,
    Bool,
    Text,
    /// A finite number in the interval.
    Number(f64, f64),
    Record(Vec<(String, Shape)>),
    List(Vec<Shape>),
    /// A list of unknown length whose every element has the shape.
    ListOf(Box<Shape>),
}

impl Shape {
    fn container(&self) -> bool {
        matches!(self, Self::Record(_) | Self::List(_) | Self::ListOf(_))
    }

    /// Equal but for the intervals of numbers, which a list's elements
    /// join.
    fn joined(&self, other: &Self) -> Option<Self> {
        Some(match (self, other) {
            (Self::Null, Self::Null) => Self::Null,
            (Self::Bool, Self::Bool) => Self::Bool,
            (Self::Text, Self::Text) => Self::Text,
            (Self::Number(a, b), Self::Number(c, d)) => Self::Number(a.min(*c), b.max(*d)),
            (Self::Record(a), Self::Record(b)) if a.len() == b.len() => Self::Record(
                a.iter()
                    .zip(b)
                    .map(|((ka, va), (kb, vb))| {
                        (ka == kb).then(|| Some((ka.clone(), va.joined(vb)?)))
                    })
                    .collect::<Option<Option<Vec<_>>>>()??,
            ),
            (Self::List(a), Self::List(b)) if a.len() == b.len() => Self::List(
                a.iter()
                    .zip(b)
                    .map(|(x, y)| x.joined(y))
                    .collect::<Option<Vec<_>>>()?,
            ),
            (Self::ListOf(a), Self::ListOf(b)) => Self::ListOf(Box::new(a.joined(b)?)),
            _ => return None,
        })
    }

    /// Whether `ts.json.plain` passes every value of this shape, read as
    /// the guard reads it: breadth first, counting values.
    fn admitted(&self) -> bool {
        if !self.container() {
            return true;
        }
        let mut pending = vec![(self, 2usize)];
        let mut visits = 1usize;
        let mut next = 0;
        while let Some(&(container, depth)) = pending.get(next) {
            next += 1;
            let members: Vec<&Shape> = match container {
                Self::Record(fields) => {
                    if visits + fields.len() > MAX_VALUES {
                        return false;
                    }
                    for (key, _) in fields {
                        if key == "toJSON" || key == "brand" {
                            return false;
                        }
                        if key.as_str() >= "0" && key.as_str() < ":" {
                            return false;
                        }
                    }
                    fields.iter().map(|(_, value)| value).collect()
                }
                Self::List(items) => {
                    if visits + items.len() > MAX_VALUES {
                        return false;
                    }
                    items.iter().collect()
                }
                // Its length is not known, so neither is its count.
                _ => return false,
            };
            visits += members.len();
            for member in members {
                if member.container() {
                    if depth > MAX_DEPTH {
                        return false;
                    }
                    pending.push((member, depth + 1));
                }
            }
        }
        true
    }
}

/// How a name is bound.
enum Decl<'a> {
    /// A `const` or `let` with an initialiser.
    Init(&'a Expr),
    /// `for (let name = lo; name < hi; name += step)`.
    Counter {
        lo: f64,
        hi: f64,
    },
    /// `for (const name of list)`.
    Element(&'a str),
    Other,
}

/// Where a container-valued use of a name sits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx<'a> {
    Stringify,
    Console,
    /// The receiver of `push` on the name itself.
    Push,
    /// Iterated by a `for ... of` that binds the name.
    Iterated(&'a str),
    /// Anywhere else: a container there may be written or escape.
    Other,
}

/// A read of a name: the member path read off it, and where the read's
/// value goes.
struct Use<'a> {
    path: Vec<Step<'a>>,
    ctx: Ctx<'a>,
}

#[derive(Clone, Copy)]
enum Step<'a> {
    Field(&'a str),
    Index(&'a Expr),
}

#[derive(Default)]
struct Walk<'a> {
    decls: HashMap<&'a str, Vec<Decl<'a>>>,
    /// Writes of each name other than a counter loop's own update.
    writes: HashMap<&'a str, usize>,
    uses: HashMap<&'a str, Vec<Use<'a>>>,
    pushes: HashMap<&'a str, Vec<&'a Expr>>,
    /// Each `JSON.stringify` call with one argument, by its call's span, and
    /// the number of calls in all.
    stringify: Vec<((usize, usize), &'a Expr)>,
    calls: usize,
}

/// The `JSON.stringify` calls of a source proven to be given plain data.
#[derive(Debug, Default)]
pub(crate) struct Plain {
    proven: HashSet<(usize, usize)>,
    /// `JSON.stringify` calls in the source, proven or not.
    pub(crate) calls: usize,
    /// The form of each one-argument call's argument, and for a name the
    /// form of what binds it, for the census.
    pub(crate) forms: Vec<String>,
}

impl Plain {
    pub(crate) fn analyse(program: &Program, session: &BTreeSet<Name>) -> Self {
        let mut walk = Walk::default();
        walk.stmts(&program.statements);
        // A source that binds `JSON` or `console` itself is not proven.
        if walk.decls.contains_key("JSON") || walk.decls.contains_key("console") {
            return Self {
                proven: HashSet::new(),
                calls: walk.calls,
                forms: Vec::new(),
            };
        }
        let mut solver = Solver {
            walk: &walk,
            frozen: HashSet::new(),
            counters: HashMap::new(),
            shapes: HashMap::new(),
        };
        solver.counters(session);
        solver.freeze(session);
        let proven = walk
            .stringify
            .iter()
            .filter(|(_, arg)| solver.shape(arg).is_some_and(|shape| shape.admitted()))
            .map(|(span, _)| *span)
            .collect();
        let forms = walk
            .stringify
            .iter()
            .map(|(_, arg)| {
                let proven = solver.shape(arg).is_some_and(|shape| shape.admitted());
                let form = match arg {
                    Expr::Ident(name, _) => {
                        let bound = match walk.decls.get(name.as_str()).map(Vec::as_slice) {
                            None => "unbound here (session or global)".to_string(),
                            Some([Decl::Init(init)]) => format!("const/let = {}", form(init)),
                            Some([Decl::Element(_)]) => "for-of element".to_string(),
                            Some([Decl::Counter { .. }]) => "counter".to_string(),
                            Some([Decl::Other]) => "parameter, pattern or var".to_string(),
                            Some(_) => "declared more than once".to_string(),
                        };
                        let written = if walk.writes.contains_key(name.as_str()) {
                            ", written"
                        } else {
                            ""
                        };
                        format!("name: {bound}{written}")
                    }
                    other => form(other).to_string(),
                };
                format!("{}{form}", if proven { "PROVEN " } else { "" })
            })
            .collect();
        Self {
            proven,
            calls: walk.calls,
            forms,
        }
    }

    /// Whether the `JSON.stringify` call at `span` is proven, and lowering
    /// uses the proof.
    pub(crate) fn proves(&self, span: (usize, usize)) -> bool {
        PROVING.load(std::sync::atomic::Ordering::Relaxed) && self.proven.contains(&span)
    }

    pub(crate) fn proven(&self) -> usize {
        self.proven.len()
    }
}

struct Solver<'w, 'a> {
    walk: &'w Walk<'a>,
    frozen: HashSet<&'a str>,
    counters: HashMap<&'a str, (f64, f64)>,
    shapes: HashMap<&'a str, Shape>,
}

impl<'a> Solver<'_, 'a> {
    /// Names every binding of which is a counter loop's, written only by
    /// those loops' updates: finite, within the loops' bounds.
    fn counters(&mut self, session: &BTreeSet<Name>) {
        for (name, decls) in &self.walk.decls {
            if session.contains(&Name::new(*name)) || self.walk.writes.contains_key(name) {
                continue;
            }
            let mut range: Option<(f64, f64)> = None;
            for decl in decls {
                let Decl::Counter { lo, hi } = decl else {
                    range = None;
                    break;
                };
                range = Some(match range {
                    None => (*lo, *hi),
                    Some((a, b)) => (a.min(*lo), b.max(*hi)),
                });
            }
            if let Some(range) = range {
                self.counters.insert(name, range);
            }
        }
    }

    /// The names whose values nothing changes: candidates first, then each
    /// whose shape is unknown or a use of which may write or leak a
    /// container is dropped, until none is.
    fn freeze(&mut self, session: &BTreeSet<Name>) {
        for (name, decls) in &self.walk.decls {
            if session.contains(&Name::new(*name))
                || self.walk.writes.contains_key(name)
                || decls.len() != 1
            {
                continue;
            }
            if matches!(decls[0], Decl::Init(_) | Decl::Element(_)) {
                self.frozen.insert(name);
            }
        }
        loop {
            self.shapes.clear();
            // Shapes depend on other names' shapes: settle them.
            for _ in 0..=self.frozen.len() {
                let mut changed = false;
                let names: Vec<&'a str> = self.frozen.iter().copied().collect();
                for name in names {
                    if let Some(shape) = self.declared_shape(name)
                        && self.shapes.get(name) != Some(&shape)
                    {
                        self.shapes.insert(name, shape);
                        changed = true;
                    }
                }
                if !changed {
                    break;
                }
            }
            let dropped: Vec<&'a str> = self
                .frozen
                .iter()
                .copied()
                .filter(|name| !self.shapes.contains_key(name) || !self.uses_hold(name))
                .collect();
            if dropped.is_empty() {
                break;
            }
            for name in dropped {
                self.frozen.remove(name);
            }
        }
    }

    fn declared_shape(&self, name: &'a str) -> Option<Shape> {
        match self.walk.decls.get(name)?.first()? {
            Decl::Init(Expr::Array(elements)) if elements.is_empty() => {
                let pushes = self.walk.pushes.get(name);
                let mut element: Option<Shape> = None;
                for pushed in pushes.into_iter().flatten() {
                    let shape = self.shape(pushed)?;
                    element = Some(match element {
                        None => shape,
                        Some(known) => known.joined(&shape)?,
                    });
                }
                Some(match element {
                    None => Shape::List(Vec::new()),
                    Some(element) => Shape::ListOf(Box::new(element)),
                })
            }
            Decl::Init(init) => {
                // Only an empty list may be pushed to.
                if self.walk.pushes.contains_key(name) {
                    return None;
                }
                self.shape(init)
            }
            Decl::Element(list) => match self.shapes.get(list)? {
                Shape::ListOf(element) => Some(element.as_ref().clone()),
                Shape::List(items) => items
                    .iter()
                    .try_fold(None::<Shape>, |joined, item| {
                        Some(Some(match joined {
                            None => item.clone(),
                            Some(joined) => joined.joined(item)?,
                        }))
                    })
                    .flatten(),
                _ => None,
            },
            _ => None,
        }
    }

    /// Whether every use of a frozen name leaves its containers unwritten
    /// and unleaked.
    fn uses_hold(&self, name: &'a str) -> bool {
        let Some(shape) = self.shapes.get(name) else {
            return false;
        };
        for used in self.walk.uses.get(name).into_iter().flatten() {
            // Follow the read down the shape: a scalar read is harmless.
            let mut current = Some(shape.clone());
            for step in &used.path {
                current = match (current, step) {
                    (Some(Shape::Record(fields)), Step::Field(field)) => fields
                        .iter()
                        .find(|(key, _)| key == field)
                        .map(|(_, value)| value.clone()),
                    (Some(Shape::List(_) | Shape::ListOf(_)), Step::Field("length")) => {
                        Some(Shape::Number(0.0, FINITE))
                    }
                    (Some(Shape::List(items)), Step::Index(index)) => match self.shape(index) {
                        Some(Shape::Number(lo, hi)) if lo == hi => items
                            .get(lo as usize)
                            .cloned()
                            .filter(|_| lo >= 0.0 && lo.fract() == 0.0),
                        _ => None,
                    },
                    _ => None,
                };
                if current.as_ref().is_some_and(|shape| !shape.container()) {
                    break;
                }
            }
            if current.as_ref().is_some_and(|shape| !shape.container()) {
                continue;
            }
            let allowed = match used.ctx {
                Ctx::Stringify | Ctx::Console => true,
                Ctx::Push => used.path.is_empty(),
                Ctx::Iterated(binding) => used.path.is_empty() && self.frozen.contains(binding),
                Ctx::Other => false,
            };
            if !allowed {
                return false;
            }
        }
        true
    }

    /// The shape of every value `expr` can give, when the analysis proves
    /// one.
    fn shape(&self, expr: &Expr) -> Option<Shape> {
        Some(match expr {
            Expr::Null => Shape::Null,
            Expr::Bool(_) => Shape::Bool,
            Expr::String(_) | Expr::Template { .. } => Shape::Text,
            Expr::Number(number) if number.is_finite() => Shape::Number(*number, *number),
            Expr::Ident(name, _) => {
                if let Some((lo, hi)) = self.counters.get(name.as_str()) {
                    Shape::Number(*lo, *hi)
                } else if self.frozen.contains(name.as_str()) {
                    self.shapes.get(name.as_str())?.clone()
                } else {
                    return None;
                }
            }
            Expr::As { value, .. } => self.shape(value)?,
            Expr::Unary { op, value } => match op {
                UnaryOp::Not => Shape::Bool,
                UnaryOp::TypeOf => Shape::Text,
                UnaryOp::Minus => match self.shape(value)? {
                    Shape::Number(lo, hi) => Shape::Number(-hi, -lo),
                    _ => return None,
                },
                UnaryOp::Plus => match self.shape(value)? {
                    number @ Shape::Number(..) => number,
                    _ => return None,
                },
                _ => return None,
            },
            Expr::Binary {
                left, op, right, ..
            } => match op {
                BinaryOp::StrictEqual
                | BinaryOp::StrictNotEqual
                | BinaryOp::LooseEqual
                | BinaryOp::LooseNotEqual
                | BinaryOp::Less
                | BinaryOp::LessEqual
                | BinaryOp::Greater
                | BinaryOp::GreaterEqual => Shape::Bool,
                BinaryOp::Add
                | BinaryOp::Subtract
                | BinaryOp::Multiply
                | BinaryOp::Divide
                | BinaryOp::Remainder => {
                    let (left, right) = (self.shape(left)?, self.shape(right)?);
                    match (op, left, right) {
                        (BinaryOp::Add, Shape::Text, other)
                        | (BinaryOp::Add, other, Shape::Text)
                            if !other.container() =>
                        {
                            Shape::Text
                        }
                        (_, Shape::Number(a, b), Shape::Number(c, d)) => {
                            let (lo, hi) = match op {
                                BinaryOp::Add => (a + c, b + d),
                                BinaryOp::Subtract => (a - d, b - c),
                                BinaryOp::Multiply => {
                                    let products = [a * c, a * d, b * c, b * d];
                                    (
                                        products.iter().copied().fold(f64::INFINITY, f64::min),
                                        products.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                                    )
                                }
                                // Only by a known divisor that is not zero.
                                BinaryOp::Divide if c == d && c != 0.0 => {
                                    let (x, y) = (a / c, b / c);
                                    (x.min(y), x.max(y))
                                }
                                BinaryOp::Remainder if c == d && c != 0.0 => (-c.abs(), c.abs()),
                                _ => return None,
                            };
                            if !(lo.abs() < FINITE && hi.abs() < FINITE) {
                                return None;
                            }
                            Shape::Number(lo, hi)
                        }
                        _ => return None,
                    }
                }
                _ => return None,
            },
            Expr::Object(properties) => {
                let mut fields: Vec<(String, Shape)> = Vec::with_capacity(properties.len());
                for property in properties {
                    let ObjectProperty::KeyValue(PropertyKey::Static(key), value) = property else {
                        return None;
                    };
                    if fields.iter().any(|(known, _)| known == key) {
                        return None;
                    }
                    fields.push((key.clone(), self.shape(value)?));
                }
                Shape::Record(fields)
            }
            Expr::Array(elements) => Shape::List(
                elements
                    .iter()
                    .map(|element| match element {
                        ArrayElement::Value(value) => self.shape(value),
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>()?,
            ),
            Expr::Member {
                object, property, ..
            } => match (self.shape(object)?, property) {
                (Shape::Record(fields), MemberProperty::Field(field)) => fields
                    .into_iter()
                    .find(|(key, _)| key == field)
                    .map(|(_, value)| value)?,
                (Shape::List(items), MemberProperty::Field(field)) if field == "length" => {
                    Shape::Number(items.len() as f64, items.len() as f64)
                }
                (Shape::ListOf(_), MemberProperty::Field(field)) if field == "length" => {
                    Shape::Number(0.0, FINITE / 2.0)
                }
                _ => return None,
            },
            _ => return None,
        })
    }
}

/// An expression's form, for the census.
fn form(expr: &Expr) -> &'static str {
    match expr {
        Expr::Null | Expr::Bool(_) | Expr::Number(_) | Expr::String(_) | Expr::Template { .. } => {
            "scalar literal"
        }
        Expr::Object(_) => "object literal",
        Expr::Array(_) => "array literal",
        Expr::Ident(..) => "name",
        Expr::Member { .. } | Expr::OptionalChain { .. } => "member read",
        Expr::Await { .. } => "await",
        Expr::Call { .. } | Expr::New { .. } => "call",
        Expr::As { value, .. } => form(value),
        _ => "other expression",
    }
}

/// The name an expression is, when it is one.
fn ident(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Ident(name, _) => Some(name),
        _ => None,
    }
}

impl<'a> Walk<'a> {
    fn declare(&mut self, name: &'a str, decl: Decl<'a>) {
        self.decls.entry(name).or_default().push(decl);
    }

    fn write(&mut self, name: &'a str) {
        *self.writes.entry(name).or_default() += 1;
    }

    /// Declares every name a pattern binds, as something the analysis
    /// knows nothing of, and walks its defaults and member targets.
    fn pattern(&mut self, pattern: &'a Pattern, write: bool) {
        match pattern {
            Pattern::Ident(name, _) => {
                if write {
                    self.write(name);
                } else {
                    self.declare(name, Decl::Other);
                }
            }
            Pattern::Rest(inner) => self.pattern(inner, write),
            Pattern::Member { object, property } => {
                self.member(object, property, Ctx::Other);
            }
            Pattern::Assign { target, default } => {
                self.pattern(target, write);
                self.expr(default, Ctx::Other);
            }
            Pattern::Array { elements, rest } => {
                for element in elements.iter().flatten() {
                    self.pattern(element, write);
                }
                if let Some(rest) = rest {
                    self.pattern(rest, write);
                }
            }
            Pattern::Object { properties, rest } => {
                for property in properties {
                    if let PropertyKey::Computed(key) = &property.key {
                        self.expr(key, Ctx::Other);
                    }
                    self.pattern(&property.value, write);
                }
                if let Some(rest) = rest {
                    self.pattern(rest, write);
                }
            }
        }
    }

    fn stmts(&mut self, stmts: &'a [Stmt]) {
        for stmt in stmts {
            self.stmt(stmt);
        }
    }

    fn stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::Spanned(_, inner) => self.stmt(inner),
            Stmt::Labeled { stmt, .. } => self.stmt(stmt),
            Stmt::Empty | Stmt::TypeAlias { .. } | Stmt::Break | Stmt::Continue => {}
            Stmt::Expr(expr) | Stmt::Throw(expr) => self.expr(expr, Ctx::Other),
            Stmt::Block(stmts) => self.stmts(stmts),
            Stmt::Var { kind, declarations } => {
                for declaration in declarations {
                    match (&declaration.pattern, &declaration.init, kind) {
                        (Pattern::Ident(name, _), Some(init), VarKind::Const | VarKind::Let) => {
                            self.declare(name, Decl::Init(init));
                            self.expr(init, Ctx::Other);
                        }
                        (pattern, init, _) => {
                            self.pattern(pattern, false);
                            if let Some(init) = init {
                                self.expr(init, Ctx::Other);
                            }
                        }
                    }
                }
            }
            Stmt::Enum { name, members } => {
                self.declare(name, Decl::Other);
                for member in members {
                    self.expr(&member.value, Ctx::Other);
                }
            }
            Stmt::Function { name, function } => {
                self.declare(name, Decl::Other);
                self.function(function);
            }
            Stmt::Return(value) => {
                if let Some(value) = value {
                    self.expr(value, Ctx::Other);
                }
            }
            Stmt::If {
                test,
                consequent,
                alternate,
            } => {
                self.expr(test, Ctx::Other);
                self.stmt(consequent);
                if let Some(alternate) = alternate {
                    self.stmt(alternate);
                }
            }
            Stmt::While { test, body } | Stmt::DoWhile { body, test, .. } => {
                self.expr(test, Ctx::Other);
                self.stmt(body);
            }
            Stmt::For {
                init,
                test,
                update,
                body,
            } => {
                let counter = counter_loop(init.as_deref(), test.as_ref(), update.as_ref());
                match (counter, init) {
                    (Some((name, lo, hi)), _) => {
                        self.declare(name, Decl::Counter { lo, hi });
                        // The test reads the counter; the update is the
                        // loop's own.
                    }
                    (None, Some(init)) => self.stmt(init),
                    (None, None) => {}
                }
                if counter.is_none() {
                    if let Some(test) = test {
                        self.expr(test, Ctx::Other);
                    }
                    if let Some(update) = update {
                        self.expr(update, Ctx::Other);
                    }
                }
                self.stmt(body);
            }
            Stmt::ForOf {
                pattern,
                kind,
                iterable,
                body,
            } => {
                match (pattern, kind, ident(iterable)) {
                    (
                        Pattern::Ident(binding, _),
                        Some(VarKind::Const | VarKind::Let),
                        Some(list),
                    ) => {
                        self.declare(binding, Decl::Element(list));
                        self.uses.entry(list).or_default().push(Use {
                            path: Vec::new(),
                            ctx: Ctx::Iterated(binding),
                        });
                    }
                    _ => {
                        self.pattern(pattern, kind.is_none());
                        self.expr(iterable, Ctx::Other);
                    }
                }
                self.stmt(body);
            }
            Stmt::ForIn {
                pattern,
                kind,
                object,
                body,
            } => {
                self.pattern(pattern, kind.is_none());
                self.expr(object, Ctx::Other);
                self.stmt(body);
            }
            Stmt::Switch {
                discriminant,
                cases,
            } => {
                self.expr(discriminant, Ctx::Other);
                for case in cases {
                    if let Some(test) = &case.test {
                        self.expr(test, Ctx::Other);
                    }
                    self.stmts(&case.consequent);
                }
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                self.stmts(body);
                if let Some(catch) = catch {
                    if let Some(binding) = &catch.binding {
                        self.pattern(binding, false);
                    }
                    self.stmts(&catch.body);
                }
                if let Some(finally) = finally {
                    self.stmts(finally);
                }
            }
        }
    }

    fn function(&mut self, function: &'a Function) {
        if let Some(name) = &function.name {
            self.declare(name, Decl::Other);
        }
        for param in &function.params {
            self.pattern(param, false);
        }
        match &function.body {
            FunctionBody::Block(stmts) => self.stmts(stmts),
            FunctionBody::Expression(body) => self.expr(body, Ctx::Other),
        }
    }

    /// A member read in context `ctx`: the use of the name at its root,
    /// with the path read off it.
    fn member(&mut self, object: &'a Expr, property: &'a MemberProperty, ctx: Ctx<'a>) {
        let mut path = vec![match property {
            MemberProperty::Field(field) => Step::Field(field),
            MemberProperty::Index(index) => Step::Index(index),
        }];
        let mut root = object;
        while let Expr::Member {
            object, property, ..
        } = root
        {
            path.push(match property {
                MemberProperty::Field(field) => Step::Field(field),
                MemberProperty::Index(index) => Step::Index(index),
            });
            root = object;
        }
        path.reverse();
        for step in &path {
            if let Step::Index(index) = step {
                self.expr(index, Ctx::Other);
            }
        }
        match ident(root) {
            Some(name) => self.uses.entry(name).or_default().push(Use { path, ctx }),
            None => self.expr(root, Ctx::Other),
        }
    }

    fn args(&mut self, args: &'a [CallArg], ctx: Ctx<'a>) {
        for arg in args {
            match arg {
                CallArg::Value(value) => self.expr(value, ctx),
                CallArg::Spread(value) => self.expr(value, Ctx::Other),
            }
        }
    }

    fn expr(&mut self, expr: &'a Expr, ctx: Ctx<'a>) {
        match expr {
            Expr::Null
            | Expr::Bool(_)
            | Expr::Number(_)
            | Expr::String(_)
            | Expr::RegExp { .. }
            | Expr::This
            | Expr::LoneSurrogateString => {}
            Expr::Ident(name, _) => self.uses.entry(name).or_default().push(Use {
                path: Vec::new(),
                ctx,
            }),
            Expr::Array(elements) => {
                for element in elements {
                    match element {
                        ArrayElement::Value(value) | ArrayElement::Spread(value) => {
                            self.expr(value, Ctx::Other);
                        }
                        ArrayElement::Hole => {}
                    }
                }
            }
            Expr::Object(properties) => {
                for property in properties {
                    match property {
                        ObjectProperty::KeyValue(key, value) => {
                            if let PropertyKey::Computed(key) = key {
                                self.expr(key, Ctx::Other);
                            }
                            self.expr(value, Ctx::Other);
                        }
                        ObjectProperty::Spread(value) => self.expr(value, Ctx::Other),
                    }
                }
            }
            Expr::Assign { target, op, value } => {
                self.target(target);
                let _: &AssignOp = op;
                self.expr(value, Ctx::Other);
            }
            Expr::Update { target, .. } => self.target(target),
            Expr::Member {
                object, property, ..
            } => self.member(object, property, ctx),
            Expr::Unary { value, .. } | Expr::Await { value, .. } | Expr::As { value, .. } => {
                self.expr(value, Ctx::Other);
            }
            Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
                self.expr(left, Ctx::Other);
                self.expr(right, Ctx::Other);
            }
            Expr::Conditional {
                test,
                consequent,
                alternate,
            } => {
                self.expr(test, Ctx::Other);
                self.expr(consequent, Ctx::Other);
                self.expr(alternate, Ctx::Other);
            }
            Expr::Template { expressions, .. } => {
                for expression in expressions {
                    self.expr(expression, Ctx::Other);
                }
            }
            Expr::Function(function) => self.function(function),
            Expr::Call { callee, args, span } => self.call(callee, args, (span.start, span.end)),
            Expr::New { args, .. } => self.args(args, Ctx::Other),
            Expr::OptionalChain { base, operations } => {
                self.expr(base, Ctx::Other);
                for operation in operations {
                    match operation {
                        OptionalOperation::Member {
                            property: MemberProperty::Index(index),
                            ..
                        } => self.expr(index, Ctx::Other),
                        OptionalOperation::Member { .. } => {}
                        OptionalOperation::Call { args, .. } => self.args(args, Ctx::Other),
                    }
                }
            }
            Expr::Delete { object, property } => self.member(object, property, Ctx::Other),
        }
    }

    fn target(&mut self, target: &'a AssignTarget) {
        match target {
            AssignTarget::Ident(name) | AssignTarget::ParenIdent(name) => self.write(name),
            AssignTarget::Member { object, property } => self.member(object, property, Ctx::Other),
            AssignTarget::Pattern(pattern) => self.pattern(pattern, true),
        }
    }

    fn call(&mut self, callee: &'a Expr, args: &'a [CallArg], span: (usize, usize)) {
        if let Expr::Member {
            object,
            property: MemberProperty::Field(method),
            ..
        } = callee
        {
            match (ident(object), method.as_str()) {
                (Some("JSON"), "stringify") => {
                    self.calls += 1;
                    if let [CallArg::Value(value)] = args {
                        self.stringify.push((span, value));
                        self.expr(value, Ctx::Stringify);
                    } else {
                        self.args(args, Ctx::Other);
                    }
                    return;
                }
                (Some("console"), "log") => {
                    self.args(args, Ctx::Console);
                    return;
                }
                (Some(list), "push") => {
                    self.uses.entry(list).or_default().push(Use {
                        path: Vec::new(),
                        ctx: Ctx::Push,
                    });
                    for arg in args {
                        match arg {
                            CallArg::Value(value) => {
                                self.pushes.entry(list).or_default().push(value);
                                self.expr(value, Ctx::Other);
                            }
                            CallArg::Spread(value) => {
                                // A spread push has no shape this follows.
                                self.write(list);
                                self.expr(value, Ctx::Other);
                            }
                        }
                    }
                    return;
                }
                _ => {}
            }
        }
        self.expr(callee, Ctx::Other);
        self.args(args, Ctx::Other);
    }
}

/// `for (let name = lo; name < hi; name++)` (or `<=`, `++name`,
/// `name += step` with a positive step): the counter, its first value and
/// a bound on every value it holds.
fn counter_loop<'a>(
    init: Option<&'a Stmt>,
    test: Option<&'a Expr>,
    update: Option<&'a Expr>,
) -> Option<(&'a str, f64, f64)> {
    let Stmt::Var {
        kind: VarKind::Let,
        declarations,
    } = init?.unlabeled()
    else {
        return None;
    };
    let [declaration] = declarations.as_slice() else {
        return None;
    };
    let (Pattern::Ident(name, _), Some(Expr::Number(lo))) =
        (&declaration.pattern, &declaration.init)
    else {
        return None;
    };
    let Expr::Binary {
        left,
        op: BinaryOp::Less | BinaryOp::LessEqual,
        right,
        ..
    } = test?
    else {
        return None;
    };
    let (Some(tested), Expr::Number(hi)) = (ident(left), right.as_ref()) else {
        return None;
    };
    let step = match update? {
        Expr::Update {
            target: AssignTarget::Ident(target),
            delta,
            ..
        } if target == name => *delta,
        Expr::Assign {
            target: AssignTarget::Ident(target),
            op: AssignOp::Binary(BinaryOp::Add),
            value,
        } if target == name => match value.as_ref() {
            Expr::Number(step) => *step,
            _ => return None,
        },
        _ => return None,
    };
    (tested == name
        && step > 0.0
        && lo.is_finite()
        && hi.is_finite()
        && step.is_finite()
        && lo.abs() < FINITE
        && hi.abs() < FINITE)
        .then_some((name.as_str(), lo.min(*hi), lo.max(*hi) + step))
}
