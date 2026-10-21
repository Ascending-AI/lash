//! The kernel's forms as one closed tree (`K-FORM-001`).
//!
//! A form that may pause a task, or call a non-native library function,
//! is an action. Every action has an [`Expr::Act`] spelling with expression
//! arguments; `do` always holds an expression. A whole right-hand side with
//! [`Atom`] arguments uses [`Rhs::Action`]. A task may pause at either
//! (`K-EVAL-008`). Validation checks that [`Expr::Call`] names a function
//! with a native implementation.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::function_value::StartAdapter;
use crate::number::{Float, Integer};
use crate::value::Bytes;
use lash_vocabulary::legacy_names::EffectName;
use lash_vocabulary::legacy_names::FunctionId;
use lash_vocabulary::legacy_names::Name;
use lash_vocabulary::types::Type;

/// A sequence of statements, run in order.
pub type Block = Vec<Stmt>;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Stmt {
    /// Declares a variable in the enclosing block and gives it a value.
    Let {
        name: Name,
        value: Rhs,
    },
    /// Writes a variable, a field or an index, evaluating its parts in
    /// `order` (`K-EVAL-003`).
    Assign {
        place: Place,
        value: Rhs,
        #[serde(default, skip_serializing_if = "AssignOrder::is_value_first")]
        order: AssignOrder,
    },
    /// Removes a field of a record, an element of a list, an entry of a map
    /// or a member of a set.
    Remove {
        member: Member,
    },
    /// Evaluates an expression for effect and discards its result.
    Do {
        value: Expr,
    },
    If {
        condition: Expr,
        then_block: Block,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        else_block: Block,
    },
    /// Runs `body` once per element of a collection, binding each to
    /// `binding`, then `else_block` unless a `break` left the loop. With
    /// `via`, a dialect's protocol opens the iterated value and steps it
    /// (`K-ITER-005`).
    For {
        /// Declared units at every continuation test, even in library code.
        /// Zero leaves the loop covered by the call's completion formula.
        #[serde(default, skip_serializing_if = "is_zero")]
        metered: u64,
        binding: Name,
        iterable: Expr,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via: Option<Protocol>,
        body: Block,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        else_block: Block,
    },
    /// Runs `body` while `condition` holds, then `else_block` unless a
    /// `break` left the loop.
    While {
        /// As the declared loop charge of [`Stmt::For`].
        #[serde(default, skip_serializing_if = "is_zero")]
        metered: u64,
        condition: Expr,
        body: Block,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        else_block: Block,
    },
    Break,
    Continue,
    Return {
        value: Expr,
    },
    /// Evaluates the value, unwinds source cleanup, then calls `policy`
    /// with the surviving value. The policy may wait; its result returns
    /// directly, outside the source function's handlers.
    ReturnVia {
        value: Expr,
        policy: FunctionId,
    },
    Try(TryStmt),
    Throw {
        value: Expr,
    },
    Print {
        value: Expr,
    },
    /// Ends the run with a result.
    Finish {
        value: Expr,
    },
    /// Ends the run as failed, with a reason.
    Fail {
        value: Expr,
    },
}

/// A dialect's iteration protocol: the library functions a `for … via`
/// calls at its header, by identity (`K-ITER-005`). Each is listed in the
/// manifest as any library function a document reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Protocol {
    /// Takes the iterated value to what the loop walks: a list, a tuple, a
    /// map, a set or a text, which the loop's own cursor walks, or any
    /// other value, a driver that `next` steps.
    pub open: FunctionId,
    /// Steps a driver: an empty tuple when it is exhausted, else a tuple of
    /// one item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<FunctionId>,
    /// Takes what was walked and an item of it to the value bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<FunctionId>,
    /// Checks a map or a set against its size when the loop opened it and
    /// the items it may still give, after each step; gives that budget
    /// anew.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<FunctionId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TryStmt {
    pub body: Block,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catch: Option<Catch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finally: Option<Block>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Catch {
    /// The variable the caught error value is bound to.
    pub binding: Name,
    pub body: Block,
}

/// The right-hand side of a `let` or an assignment.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Rhs {
    Expr(Expr),
    Action(Action),
}

/// An action as the whole right-hand side of its own statement, its
/// arguments atoms (`K-FORM-001`); [`Expr::Act`] nests one in an
/// expression.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    /// Calls a function and yields what it returns.
    Call { callee: Callee, args: Vec<Atom> },
    /// Runs one effect and yields its result, decoded as `result`.
    Perform {
        effect: EffectName,
        args: Vec<Atom>,
        result: Type,
        /// Whether the task takes one more turn of the ready queue once
        /// the outcome is in, before it continues (`K-TASK-005`).
        #[serde(default, skip_serializing_if = "is_false")]
        then_yield: bool,
    },
    /// Waits for a number of milliseconds.
    Sleep {
        duration: Atom,
        /// As [`Action::Perform`]'s.
        #[serde(default, skip_serializing_if = "is_false")]
        then_yield: bool,
    },
    /// Waits on one task handle.
    Join { task: Atom },
    /// Waits on a list of task handles.
    JoinMany { mode: JoinMode, tasks: Atom },
    /// Lets every other ready task run first.
    Yield,
    /// Starts a task running a function and yields its handle.
    Spawn { callee: Callee, args: Vec<Atom> },
    /// Raises a cancellation in a task at its current wait.
    Cancel { task: Atom },
    /// Awaits a value by its dialect's await policy: a library function
    /// that takes the value, waits as the dialect's `await` does, and
    /// yields what the `await` gives (`K-TASK-026`).
    Await { policy: FunctionId, value: Atom },
}

fn is_false(flag: &bool) -> bool {
    !*flag
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum JoinMode {
    /// Every member has ended, or the first failure.
    All,
    /// Every member has ended.
    AllSettled,
    /// The first member to end.
    Race,
    /// The first member to succeed.
    Any,
}

/// What a call or a `spawn` runs.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Callee {
    /// A declared function of the document, by name.
    Declared(Name),
    /// The closure or function reference a variable holds.
    Value(Name),
    /// A library function, by identity.
    Library(FunctionId),
}

/// An argument of an action: a variable or a literal, never a computation.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Atom {
    Variable(Name),
    Literal(Literal),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Literal {
    Null,
    Absent,
    Bool(bool),
    Int(Integer),
    Float(Float),
    Text(String),
    Bytes(Bytes),
    /// A reference to a declared function of the document.
    Function(Name),
}

/// An expression, evaluated within one statement. Only an [`Expr::Act`]
/// in it may pause the task (`K-EVAL-008`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Expr {
    Literal(Literal),
    Variable(Name),
    /// Evaluates the condition, then only the selected branch.
    If {
        cond: Box<Expr>,
        then: Box<Expr>,
        r#else: Box<Expr>,
    },
    /// Evaluates `value` before binding `name`, visible only in `body`.
    Let {
        name: Name,
        value: Box<Expr>,
        body: Box<Expr>,
    },
    Tuple(Vec<Expr>),
    List(Vec<Expr>),
    Map(Vec<MapEntry>),
    Set(Vec<Expr>),
    Record(Vec<RecordEntry>),
    /// Reads a field or an index.
    Member(Box<Member>),
    Closure(Box<Closure>),
    /// Calls a library function that has a native implementation.
    Call {
        function: FunctionId,
        args: Vec<Expr>,
    },
    /// The embedder's clock, as a timestamp.
    Clock,
    /// A float drawn by the embedder, uniform in `[0, 1)`.
    Random,
    /// Reads through a projection handle.
    Read(Box<ProjectionRead>),
    /// An action nested in the expression: the statement may pause at it,
    /// the operands it has already evaluated kept as the roles of its
    /// continuation (`K-EVAL-008`).
    Act(Box<ActExpr>),
}

/// An action inside an expression: the action forms that may nest, with
/// expression arguments.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ActExpr {
    pub kind: ActKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<Expr>,
}

/// What an [`Expr::Act`] does. `yield` and `cancel` stay statement
/// actions.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ActKind {
    /// Calls a function and yields what it returns.
    Call(Callee),
    /// Runs one effect and yields its result, decoded as `result`.
    Perform {
        effect: EffectName,
        result: Type,
        /// As [`Action::Perform`]'s.
        #[serde(default, skip_serializing_if = "is_false")]
        then_yield: bool,
    },
    /// Waits for a number of milliseconds, its one argument.
    Sleep {
        /// As [`Action::Perform`]'s.
        #[serde(default, skip_serializing_if = "is_false")]
        then_yield: bool,
    },
    /// Starts a task running a function and yields its handle.
    Spawn(Callee),
    /// Waits on the task handle that is its one argument.
    Join,
    /// Waits on the list of task handles that is its one argument.
    JoinMany(JoinMode),
    /// Gives other ready tasks their turn.
    Yield,
    /// Cancels the task handle that is its one argument.
    Cancel,
    /// Awaits its one argument by the await policy (`K-TASK-026`).
    Await(FunctionId),
}

impl ActExpr {
    /// The statement [`Action`] this nested action spells when every
    /// argument is a variable or a literal: kernel text writes such an
    /// action, as a whole right-hand side, as the statement form.
    pub fn as_action(&self) -> Option<Action> {
        let args = self
            .args
            .iter()
            .map(|expr| match expr {
                Expr::Variable(name) => Some(Atom::Variable(name.clone())),
                Expr::Literal(literal) => Some(Atom::Literal(literal.clone())),
                _ => None,
            })
            .collect::<Option<Vec<Atom>>>()?;
        Some(match &self.kind {
            ActKind::Call(callee) => Action::Call {
                callee: callee.clone(),
                args,
            },
            ActKind::Spawn(callee) => Action::Spawn {
                callee: callee.clone(),
                args,
            },
            ActKind::Perform {
                effect,
                result,
                then_yield,
            } => Action::Perform {
                effect: effect.clone(),
                args,
                result: result.clone(),
                then_yield: *then_yield,
            },
            ActKind::Sleep { then_yield } => Action::Sleep {
                duration: one(args)?,
                then_yield: *then_yield,
            },
            ActKind::Yield if args.is_empty() => Action::Yield,
            ActKind::Yield => return None,
            ActKind::Cancel => Action::Cancel { task: one(args)? },
            ActKind::Join => Action::Join { task: one(args)? },
            ActKind::JoinMany(mode) => Action::JoinMany {
                mode: *mode,
                tasks: one(args)?,
            },
            ActKind::Await(policy) => Action::Await {
                policy: *policy,
                value: one(args)?,
            },
        })
    }
}

fn one(args: Vec<Atom>) -> Option<Atom> {
    let [only] = <[Atom; 1]>::try_from(args).ok()?;
    Some(only)
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MapEntry {
    pub key: Expr,
    pub value: Expr,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordEntry {
    pub field: String,
    pub value: Expr,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectionRead {
    pub handle: Expr,
    /// What is asked of the projection: kernel data the host interprets.
    pub request: Expr,
}

/// A field of a record or an index into a collection.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Member {
    Field { target: Expr, field: String },
    Index { target: Expr, index: Expr },
}

/// Where an assignment writes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Place {
    Variable(Name),
    Member(Member),
}

/// The order an assignment to a member evaluates its parts in
/// (`K-EVAL-003`): each dialect's own, written in the document.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AssignOrder {
    /// The right-hand side, then the place's target, then its index, as
    /// Python assigns.
    #[default]
    ValueFirst,
    /// The place's target, then its index, then the right-hand side, as
    /// TypeScript assigns. Only a member is written in this order, and its
    /// right-hand side is an expression: an action there is nested.
    PlaceFirst,
}

impl AssignOrder {
    pub fn is_value_first(&self) -> bool {
        *self == Self::ValueFirst
    }
}

/// A function value written in place. It shares the variables of the scope
/// that defines it (`K-CLO-001`); which ones is derived, never written.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Closure {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<Name>,
    pub body: Block,
    /// How a value this closure makes is started when it crosses to a tool
    /// as a [`crate::FunctionValue`] (`K-CLO-004`); `None` for one that
    /// cannot be started. Executable behaviour: it is part of the
    /// document's identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<StartAdapter>,
}

/// A declared function's parameters and body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Function {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<Name>,
    pub body: Block,
    /// How a reference to this function is started when it crosses to a
    /// tool, as [`Closure::start`] says for a closure: a saved function
    /// keeps the adapter of the closure it was saved from. An entry needs
    /// none; its signature and kernel convention are its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<StartAdapter>,
}

/// Any node of the tree, for a walk that does not care which.
#[derive(Clone, Copy, Debug)]
pub enum Node<'a> {
    Block(&'a [Stmt]),
    Stmt(&'a Stmt),
    Action(&'a Action),
    Expr(&'a Expr),
}

impl<'a> Node<'a> {
    /// The node's children, in the order kernel text writes them. This
    /// order is the one a [`Site`] counts in, and it is part of the kernel
    /// version (`K-ID-004`).
    ///
    /// A block's children are its statements. A statement's are its
    /// expressions, its action and its blocks, in the order they are
    /// written. A statement's action has none: its arguments are atoms. A
    /// nested action's are its arguments. A closure's one child is its
    /// body.
    pub fn children(self) -> Vec<Node<'a>> {
        let mut out = Vec::new();
        match self {
            Node::Block(block) => out.extend(block.iter().map(Node::Stmt)),
            Node::Action(_) => {}
            Node::Stmt(stmt) => match stmt {
                Stmt::Let { value, .. } => out.push(rhs_node(value)),
                Stmt::Assign { place, value, .. } => {
                    if let Place::Member(member) = place {
                        push_member(member, &mut out);
                    }
                    out.push(rhs_node(value));
                }
                Stmt::Remove { member } => push_member(member, &mut out),
                Stmt::Do { value } => out.push(Node::Expr(value)),
                Stmt::If {
                    condition,
                    then_block,
                    else_block,
                } => {
                    out.push(Node::Expr(condition));
                    out.push(Node::Block(then_block));
                    out.push(Node::Block(else_block));
                }
                // An empty `else` is no child, so a loop without one has
                // the sites it always had.
                Stmt::For {
                    iterable,
                    body,
                    else_block,
                    ..
                } => {
                    out.push(Node::Expr(iterable));
                    out.push(Node::Block(body));
                    if !else_block.is_empty() {
                        out.push(Node::Block(else_block));
                    }
                }
                Stmt::While {
                    condition,
                    body,
                    else_block,
                    ..
                } => {
                    out.push(Node::Expr(condition));
                    out.push(Node::Block(body));
                    if !else_block.is_empty() {
                        out.push(Node::Block(else_block));
                    }
                }
                Stmt::Break | Stmt::Continue => {}
                Stmt::Return { value }
                | Stmt::ReturnVia { value, .. }
                | Stmt::Throw { value }
                | Stmt::Print { value }
                | Stmt::Finish { value }
                | Stmt::Fail { value } => out.push(Node::Expr(value)),
                Stmt::Try(scope) => {
                    out.push(Node::Block(&scope.body));
                    if let Some(catch) = &scope.catch {
                        out.push(Node::Block(&catch.body));
                    }
                    if let Some(finally) = &scope.finally {
                        out.push(Node::Block(finally));
                    }
                }
            },
            Node::Expr(expr) => match expr {
                Expr::Literal(_) | Expr::Variable(_) | Expr::Clock | Expr::Random => {}
                Expr::Tuple(items) | Expr::List(items) | Expr::Set(items) => {
                    out.extend(items.iter().map(Node::Expr));
                }
                Expr::Map(entries) => {
                    for entry in entries {
                        out.push(Node::Expr(&entry.key));
                        out.push(Node::Expr(&entry.value));
                    }
                }
                Expr::Record(entries) => {
                    out.extend(entries.iter().map(|entry| Node::Expr(&entry.value)));
                }
                Expr::Member(member) => push_member(member, &mut out),
                Expr::Closure(closure) => out.push(Node::Block(&closure.body)),
                Expr::Call { args, .. } => out.extend(args.iter().map(Node::Expr)),
                Expr::If { cond, then, r#else } => {
                    out.extend([Node::Expr(cond), Node::Expr(then), Node::Expr(r#else)]);
                }
                Expr::Let { value, body, .. } => {
                    out.extend([Node::Expr(value), Node::Expr(body)]);
                }
                Expr::Read(read) => {
                    out.push(Node::Expr(&read.handle));
                    out.push(Node::Expr(&read.request));
                }
                Expr::Act(act) => out.extend(act.args.iter().map(Node::Expr)),
            },
        }
        out
    }
}

fn rhs_node(rhs: &Rhs) -> Node<'_> {
    match rhs {
        Rhs::Expr(expr) => Node::Expr(expr),
        Rhs::Action(action) => Node::Action(action),
    }
}

fn push_member<'a>(member: &'a Member, out: &mut Vec<Node<'a>>) {
    match member {
        Member::Field { target, .. } => out.push(Node::Expr(target)),
        Member::Index { target, index } => {
            out.push(Node::Expr(target));
            out.push(Node::Expr(index));
        }
    }
}

/// Any node of the tree, for writing: [`Node`]'s mutable spelling, with
/// the same children in the same order.
pub enum NodeMut<'a> {
    Block(&'a mut Block),
    Stmt(&'a mut Stmt),
    Action(&'a mut Action),
    Expr(&'a mut Expr),
}

impl<'a> NodeMut<'a> {
    /// The node, for reading.
    pub fn as_node(&self) -> Node<'_> {
        match self {
            Self::Block(block) => Node::Block(block),
            Self::Stmt(stmt) => Node::Stmt(stmt),
            Self::Action(action) => Node::Action(action),
            Self::Expr(expr) => Node::Expr(expr),
        }
    }

    /// The node's children, in the order of [`Node::children`]
    /// (`K-ID-004`).
    pub fn children(self) -> Vec<NodeMut<'a>> {
        let mut out = Vec::new();
        match self {
            Self::Block(block) => out.extend(block.iter_mut().map(NodeMut::Stmt)),
            Self::Action(_) => {}
            Self::Stmt(stmt) => match stmt {
                Stmt::Let { value, .. } => out.push(rhs_node_mut(value)),
                Stmt::Assign { place, value, .. } => {
                    if let Place::Member(member) = place {
                        push_member_mut(member, &mut out);
                    }
                    out.push(rhs_node_mut(value));
                }
                Stmt::Remove { member } => push_member_mut(member, &mut out),
                Stmt::Do { value } => out.push(NodeMut::Expr(value)),
                Stmt::If {
                    condition,
                    then_block,
                    else_block,
                } => {
                    out.push(NodeMut::Expr(condition));
                    out.push(NodeMut::Block(then_block));
                    out.push(NodeMut::Block(else_block));
                }
                // As `Node::children`: an empty `else` is no child.
                Stmt::For {
                    iterable,
                    body,
                    else_block,
                    ..
                } => {
                    out.push(NodeMut::Expr(iterable));
                    out.push(NodeMut::Block(body));
                    if !else_block.is_empty() {
                        out.push(NodeMut::Block(else_block));
                    }
                }
                Stmt::While {
                    condition,
                    body,
                    else_block,
                    ..
                } => {
                    out.push(NodeMut::Expr(condition));
                    out.push(NodeMut::Block(body));
                    if !else_block.is_empty() {
                        out.push(NodeMut::Block(else_block));
                    }
                }
                Stmt::Break | Stmt::Continue => {}
                Stmt::Return { value }
                | Stmt::ReturnVia { value, .. }
                | Stmt::Throw { value }
                | Stmt::Print { value }
                | Stmt::Finish { value }
                | Stmt::Fail { value } => out.push(NodeMut::Expr(value)),
                Stmt::Try(scope) => {
                    out.push(NodeMut::Block(&mut scope.body));
                    if let Some(catch) = &mut scope.catch {
                        out.push(NodeMut::Block(&mut catch.body));
                    }
                    if let Some(finally) = &mut scope.finally {
                        out.push(NodeMut::Block(finally));
                    }
                }
            },
            Self::Expr(expr) => match expr {
                Expr::Literal(_) | Expr::Variable(_) | Expr::Clock | Expr::Random => {}
                Expr::Tuple(items) | Expr::List(items) | Expr::Set(items) => {
                    out.extend(items.iter_mut().map(NodeMut::Expr));
                }
                Expr::Map(entries) => {
                    for entry in entries {
                        out.push(NodeMut::Expr(&mut entry.key));
                        out.push(NodeMut::Expr(&mut entry.value));
                    }
                }
                Expr::Record(entries) => {
                    out.extend(
                        entries
                            .iter_mut()
                            .map(|entry| NodeMut::Expr(&mut entry.value)),
                    );
                }
                Expr::Member(member) => push_member_mut(member, &mut out),
                Expr::Closure(closure) => out.push(NodeMut::Block(&mut closure.body)),
                Expr::Call { args, .. } => out.extend(args.iter_mut().map(NodeMut::Expr)),
                Expr::If { cond, then, r#else } => {
                    out.extend([
                        NodeMut::Expr(cond),
                        NodeMut::Expr(then),
                        NodeMut::Expr(r#else),
                    ]);
                }
                Expr::Let { value, body, .. } => {
                    out.extend([NodeMut::Expr(value), NodeMut::Expr(body)]);
                }
                Expr::Read(read) => {
                    out.push(NodeMut::Expr(&mut read.handle));
                    out.push(NodeMut::Expr(&mut read.request));
                }
                Expr::Act(act) => out.extend(act.args.iter_mut().map(NodeMut::Expr)),
            },
        }
        out
    }

    /// The node `path` reaches from this one.
    pub fn descend(self, path: &[u32]) -> Option<NodeMut<'a>> {
        let mut node = self;
        for step in path {
            node = node
                .children()
                .into_iter()
                .nth(usize::try_from(*step).ok()?)?;
        }
        Some(node)
    }
}

fn rhs_node_mut(rhs: &mut Rhs) -> NodeMut<'_> {
    match rhs {
        Rhs::Expr(expr) => NodeMut::Expr(expr),
        Rhs::Action(action) => NodeMut::Action(action),
    }
}

fn push_member_mut<'a>(member: &'a mut Member, out: &mut Vec<NodeMut<'a>>) {
    match member {
        Member::Field { target, .. } => out.push(NodeMut::Expr(target)),
        Member::Index { target, index } => {
            out.push(NodeMut::Expr(target));
            out.push(NodeMut::Expr(index));
        }
    }
}

/// Calls `visit` on every closure in `block`, outer before inner, in the
/// order its statements are written.
pub fn each_closure_mut(block: &mut [Stmt], visit: &mut dyn FnMut(&mut Closure)) {
    for stmt in block {
        stmt_closures(stmt, visit);
    }
}

fn stmt_closures(stmt: &mut Stmt, visit: &mut dyn FnMut(&mut Closure)) {
    match stmt {
        Stmt::Let { value, .. } => rhs_closures(value, visit),
        Stmt::Assign { place, value, .. } => {
            if let Place::Member(member) = place {
                member_closures(member, visit);
            }
            rhs_closures(value, visit);
        }
        Stmt::Remove { member } => member_closures(member, visit),
        Stmt::Do { value } => expr_closures(value, visit),
        Stmt::Break | Stmt::Continue => {}
        Stmt::If {
            condition,
            then_block,
            else_block,
        } => {
            expr_closures(condition, visit);
            each_closure_mut(then_block, visit);
            each_closure_mut(else_block, visit);
        }
        Stmt::For {
            iterable,
            body,
            else_block,
            ..
        } => {
            expr_closures(iterable, visit);
            each_closure_mut(body, visit);
            each_closure_mut(else_block, visit);
        }
        Stmt::While {
            condition,
            body,
            else_block,
            ..
        } => {
            expr_closures(condition, visit);
            each_closure_mut(body, visit);
            each_closure_mut(else_block, visit);
        }
        Stmt::Return { value }
        | Stmt::ReturnVia { value, .. }
        | Stmt::Throw { value }
        | Stmt::Print { value }
        | Stmt::Finish { value }
        | Stmt::Fail { value } => expr_closures(value, visit),
        Stmt::Try(scope) => {
            each_closure_mut(&mut scope.body, visit);
            if let Some(catch) = &mut scope.catch {
                each_closure_mut(&mut catch.body, visit);
            }
            if let Some(finally) = &mut scope.finally {
                each_closure_mut(finally, visit);
            }
        }
    }
}

fn rhs_closures(rhs: &mut Rhs, visit: &mut dyn FnMut(&mut Closure)) {
    if let Rhs::Expr(expr) = rhs {
        expr_closures(expr, visit);
    }
}

fn member_closures(member: &mut Member, visit: &mut dyn FnMut(&mut Closure)) {
    match member {
        Member::Field { target, .. } => expr_closures(target, visit),
        Member::Index { target, index } => {
            expr_closures(target, visit);
            expr_closures(index, visit);
        }
    }
}

fn expr_closures(expr: &mut Expr, visit: &mut dyn FnMut(&mut Closure)) {
    match expr {
        Expr::Literal(_) | Expr::Variable(_) | Expr::Clock | Expr::Random => {}
        Expr::Tuple(items) | Expr::List(items) | Expr::Set(items) => {
            items.iter_mut().for_each(|item| expr_closures(item, visit));
        }
        Expr::Map(entries) => entries.iter_mut().for_each(|entry| {
            expr_closures(&mut entry.key, visit);
            expr_closures(&mut entry.value, visit);
        }),
        Expr::Record(entries) => entries
            .iter_mut()
            .for_each(|entry| expr_closures(&mut entry.value, visit)),
        Expr::Member(member) => member_closures(member, visit),
        Expr::Closure(closure) => {
            visit(closure);
            each_closure_mut(&mut closure.body, visit);
        }
        Expr::Call { args, .. } => args.iter_mut().for_each(|arg| expr_closures(arg, visit)),
        Expr::If { cond, then, r#else } => {
            expr_closures(cond, visit);
            expr_closures(then, visit);
            expr_closures(r#else, visit);
        }
        Expr::Let { value, body, .. } => {
            expr_closures(value, visit);
            expr_closures(body, visit);
        }
        Expr::Read(read) => {
            expr_closures(&mut read.handle, visit);
            expr_closures(&mut read.request, visit);
        }
        Expr::Act(act) => act
            .args
            .iter_mut()
            .for_each(|arg| expr_closures(arg, visit)),
    }
}

/// Canonical expression spelling of a statement-position action.
impl From<Action> for Expr {
    fn from(action: Action) -> Self {
        let expr = |atom: &Atom| match atom {
            Atom::Variable(name) => Expr::Variable(name.clone()),
            Atom::Literal(literal) => Expr::Literal(literal.clone()),
        };
        let (kind, args) = match &action {
            Action::Call { callee, args } => (ActKind::Call(callee.clone()), args.iter().collect()),
            Action::Spawn { callee, args } => {
                (ActKind::Spawn(callee.clone()), args.iter().collect())
            }
            Action::Perform {
                effect,
                args,
                result,
                then_yield,
            } => (
                ActKind::Perform {
                    effect: effect.clone(),
                    result: result.clone(),
                    then_yield: *then_yield,
                },
                args.iter().collect(),
            ),
            Action::Sleep {
                duration,
                then_yield,
            } => (
                ActKind::Sleep {
                    then_yield: *then_yield,
                },
                vec![duration],
            ),
            Action::Join { task } => (ActKind::Join, vec![task]),
            Action::JoinMany { mode, tasks } => (ActKind::JoinMany(*mode), vec![tasks]),
            Action::Await { policy, value } => (ActKind::Await(*policy), vec![value]),
            Action::Yield => (ActKind::Yield, vec![]),
            Action::Cancel { task } => (ActKind::Cancel, vec![task]),
        };
        Expr::Act(Box::new(ActExpr {
            kind,
            args: args.into_iter().map(expr).collect::<Vec<_>>(),
        }))
    }
}

fn is_zero(units: &u64) -> bool {
    *units == 0
}
