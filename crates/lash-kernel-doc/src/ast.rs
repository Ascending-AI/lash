//! The kernel's forms as one closed tree (`K-FORM-001`).
//!
//! The statement rule is carried by the types where that is cheap. A form
//! that may pause a task, or that calls anything but a native library
//! function, is an [`Action`]: it is the whole right-hand side of a
//! statement and its arguments are [`Atom`]s. An [`Expr`] holds no action,
//! so "a wait nested in an expression" has no representation. What the types
//! cannot say, that an [`Expr::Call`] names a function with a native
//! implementation, is checked by validation (`K-STMT-002`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::name::{EffectName, FunctionId, Name};
use crate::number::{Float, Integer};
use crate::types::Type;
use crate::value::Bytes;

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
    /// Writes a variable, a field or an index.
    Assign {
        place: Place,
        value: Rhs,
    },
    /// Removes a field of a record, an element of a list, an entry of a map
    /// or a member of a set.
    Remove {
        member: Member,
    },
    /// Runs an action and discards its result.
    Do {
        action: Action,
    },
    If {
        condition: Expr,
        then_block: Block,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        else_block: Block,
    },
    /// Runs `body` once per element of a collection, binding each to
    /// `binding`.
    For {
        binding: Name,
        iterable: Expr,
        body: Block,
    },
    While {
        condition: Expr,
        body: Block,
    },
    Break,
    Continue,
    Return {
        value: Expr,
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

/// A form that is the whole right-hand side of its own statement
/// (`K-STMT-001`).
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
    },
    /// Waits for a number of milliseconds.
    Sleep { duration: Atom },
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

/// What a statement-position call or a `spawn` runs.
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

/// An expression: evaluated within one statement, it never pauses a task.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Expr {
    Literal(Literal),
    Variable(Name),
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

/// A function value written in place. It shares the variables of the scope
/// that defines it (`K-CLO-001`); which ones is derived, never written.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Closure {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<Name>,
    pub body: Block,
}

/// A declared function's parameters and body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Function {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<Name>,
    pub body: Block,
}

/// The code a node belongs to.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    /// The document's `main`.
    Main,
    /// A declared function of the document.
    Function(Name),
    /// The kernel-code body of a library function.
    Library(FunctionId),
}

/// A node's address: the code it belongs to and the chain of
/// [`Node::children`] indexes that reaches it from that code's body.
///
/// A site is derived from the document and is never written by a host
/// (`K-ID-004`). An empty chain addresses the body itself.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct Site {
    pub unit: Unit,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<u32>,
}

impl Site {
    pub fn new(unit: Unit, path: impl Into<Vec<u32>>) -> Self {
        Self {
            unit,
            path: path.into(),
        }
    }

    /// The site of this node's `index`-th child.
    pub fn child(&self, index: u32) -> Self {
        let mut path = self.path.clone();
        path.push(index);
        Self {
            unit: self.unit.clone(),
            path,
        }
    }
}

impl std::fmt::Display for Site {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.unit {
            Unit::Main => f.write_str("main")?,
            Unit::Function(name) => write!(f, "fn {name}")?,
            Unit::Library(id) => write!(f, "function {id}")?,
        }
        for step in &self.path {
            write!(f, "/{step}")?;
        }
        Ok(())
    }
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
    /// written. An action has none: its arguments are atoms. A closure's one
    /// child is its body.
    pub fn children(self) -> Vec<Node<'a>> {
        let mut out = Vec::new();
        match self {
            Node::Block(block) => out.extend(block.iter().map(Node::Stmt)),
            Node::Action(_) => {}
            Node::Stmt(stmt) => match stmt {
                Stmt::Let { value, .. } => out.push(rhs_node(value)),
                Stmt::Assign { place, value } => {
                    if let Place::Member(member) = place {
                        push_member(member, &mut out);
                    }
                    out.push(rhs_node(value));
                }
                Stmt::Remove { member } => push_member(member, &mut out),
                Stmt::Do { action } => out.push(Node::Action(action)),
                Stmt::If {
                    condition,
                    then_block,
                    else_block,
                } => {
                    out.push(Node::Expr(condition));
                    out.push(Node::Block(then_block));
                    out.push(Node::Block(else_block));
                }
                Stmt::For { iterable, body, .. } => {
                    out.push(Node::Expr(iterable));
                    out.push(Node::Block(body));
                }
                Stmt::While { condition, body } => {
                    out.push(Node::Expr(condition));
                    out.push(Node::Block(body));
                }
                Stmt::Break | Stmt::Continue => {}
                Stmt::Return { value }
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
                Expr::Read(read) => {
                    out.push(Node::Expr(&read.handle));
                    out.push(Node::Expr(&read.request));
                }
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
