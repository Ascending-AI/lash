//! One document walk shared by dialect printers.
//!
//! The tree retains statement boundaries and evaluation order. A spelling
//! table renders this tree, never traverses the kernel document again.

use lash_kernel_doc::{self as k, Name};

/// Language independent expressions in an imperative source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceExpr {
    Null,
    Bool(bool),
    Text(String),
    Variable(Name),
    Array(Vec<SourceExpr>),
    /// A reserved operation; its name is part of the printer/front-end contract.
    Intrinsic(&'static str, Vec<SourceExpr>),
    Function {
        params: Vec<Name>,
        body: Vec<SourceStmt>,
    },
}

/// Native imperative control with explicit kernel operations as leaves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceStmt {
    Let(Name, SourceExpr),
    Assign(SourcePlace, SourceExpr),
    Expression(SourceExpr),
    If(SourceExpr, Vec<SourceStmt>, Vec<SourceStmt>),
    For(Name, SourceExpr, Vec<SourceStmt>),
    While(SourceExpr, Vec<SourceStmt>),
    Break,
    Continue,
    Return(SourceExpr),
    Throw(SourceExpr),
    Try {
        body: Vec<SourceStmt>,
        catch: Option<(Name, Vec<SourceStmt>)>,
        finally: Option<Vec<SourceStmt>>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourcePlace {
    Variable(Name),
    /// A reserved field/index reference, written as a dialect's lvalue.
    Member(SourceExpr),
}

/// A dialect supplies spelling for expressions and statements of the shared tree.
pub trait Spelling {
    /// Operands are already spelled by the shared renderer. Function bodies
    /// arrive as one block operand; leaves have none.
    fn expression(&self, expression: &SourceExpr, operands: &[String]) -> String;
    /// Children are already spelled, in source evaluation order.
    fn statement(&self, statement: &SourceStmt, operands: &[String]) -> String;
}

/// Renders the imperative tree with a dialect's spelling table. All recursive
/// walking lives here; a dialect sees only the form and rendered children.
pub fn render_source(source: &[SourceStmt], spelling: &impl Spelling) -> String {
    source
        .iter()
        .map(|statement| render_statement(statement, spelling))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}
fn render_expression(expression: &SourceExpr, spelling: &impl Spelling) -> String {
    let operands = match expression {
        SourceExpr::Array(items) | SourceExpr::Intrinsic(_, items) => items
            .iter()
            .map(|item| render_expression(item, spelling))
            .collect(),
        SourceExpr::Function { body, .. } => vec![render_source(body, spelling)],
        _ => vec![],
    };
    spelling.expression(expression, &operands)
}
fn render_statement(statement: &SourceStmt, spelling: &impl Spelling) -> String {
    let expr = |expression: &SourceExpr| render_expression(expression, spelling);
    let block = |block: &Vec<SourceStmt>| render_source(block, spelling);
    let operands = match statement {
        SourceStmt::Let(_, value)
        | SourceStmt::Expression(value)
        | SourceStmt::Return(value)
        | SourceStmt::Throw(value) => vec![expr(value)],
        SourceStmt::Assign(place, value) => vec![
            match place {
                SourcePlace::Variable(name) => expr(&SourceExpr::Variable(name.clone())),
                SourcePlace::Member(member) => expr(member),
            },
            expr(value),
        ],
        SourceStmt::If(condition, yes, no) => vec![expr(condition), block(yes), block(no)],
        SourceStmt::For(_, iterable, body) | SourceStmt::While(iterable, body) => {
            vec![expr(iterable), block(body)]
        }
        SourceStmt::Try {
            body,
            catch,
            finally,
        } => vec![
            block(body),
            catch
                .as_ref()
                .map(|(_, body)| block(body))
                .unwrap_or_default(),
            finally.as_ref().map(block).unwrap_or_default(),
        ],
        SourceStmt::Break | SourceStmt::Continue => vec![],
    };
    spelling.statement(statement, &operands)
}

/// Walks a document once. Metadata strings contain declarations only, never code.
/// Every action stays at its original statement; no temporary is folded.
pub fn imperative_source(document: &k::Document) -> Result<Vec<SourceStmt>, k::EncodeError> {
    let mut source = vec![SourceStmt::Expression(call(
        "document",
        vec![
            text(json(&document.manifest)?),
            text(json(&document.entries)?),
            SourceExpr::Array(
                document
                    .private_bindings
                    .iter()
                    .map(|name| text(name.as_str()))
                    .collect(),
            ),
        ],
    ))];
    for (name, function) in &document.functions {
        source.push(SourceStmt::Expression(call(
            "declare",
            vec![
                text(name.as_str()),
                function_expr(&function.params, &function.body),
            ],
        )));
    }
    source.push(SourceStmt::Expression(call(
        "main",
        vec![function_expr(&[], &document.main)],
    )));
    Ok(source)
}

fn json(value: &impl serde::Serialize) -> Result<String, k::EncodeError> {
    serde_json::to_string(value).map_err(|error| k::EncodeError {
        message: error.to_string(),
    })
}
fn text(value: impl Into<String>) -> SourceExpr {
    SourceExpr::Text(value.into())
}
fn call(name: &'static str, args: Vec<SourceExpr>) -> SourceExpr {
    SourceExpr::Intrinsic(name, args)
}
fn function_expr(params: &[Name], body: &k::Block) -> SourceExpr {
    SourceExpr::Function {
        params: params.to_vec(),
        body: block(body),
    }
}
fn block(body: &k::Block) -> Vec<SourceStmt> {
    body.iter().map(statement).collect()
}
fn statement(statement: &k::Stmt) -> SourceStmt {
    match statement {
        k::Stmt::Let { name, value } => SourceStmt::Let(name.clone(), rhs(value)),
        k::Stmt::Assign { place, value } => SourceStmt::Assign(
            match place {
                k::Place::Variable(name) => SourcePlace::Variable(name.clone()),
                k::Place::Member(value) => SourcePlace::Member(member(value)),
            },
            rhs(value),
        ),
        k::Stmt::Remove { member: value } => {
            SourceStmt::Expression(call("remove", vec![member(value)]))
        }
        k::Stmt::Do { action: value } => SourceStmt::Expression(action(value)),
        k::Stmt::If {
            condition,
            then_block,
            else_block,
        } => SourceStmt::If(expression(condition), block(then_block), block(else_block)),
        k::Stmt::For {
            binding,
            iterable,
            body,
        } => SourceStmt::For(binding.clone(), expression(iterable), block(body)),
        k::Stmt::While { condition, body } => SourceStmt::While(expression(condition), block(body)),
        k::Stmt::Break => SourceStmt::Break,
        k::Stmt::Continue => SourceStmt::Continue,
        k::Stmt::Return { value } => SourceStmt::Return(expression(value)),
        k::Stmt::Throw { value } => SourceStmt::Throw(expression(value)),
        k::Stmt::Print { value } => SourceStmt::Expression(call("print", vec![expression(value)])),
        k::Stmt::Finish { value } => {
            SourceStmt::Expression(call("finish", vec![expression(value)]))
        }
        k::Stmt::Fail { value } => SourceStmt::Expression(call("fail", vec![expression(value)])),
        k::Stmt::Try(scope) => SourceStmt::Try {
            body: block(&scope.body),
            catch: scope
                .catch
                .as_ref()
                .map(|catch| (catch.binding.clone(), block(&catch.body))),
            finally: scope.finally.as_ref().map(block),
        },
    }
}
fn rhs(value: &k::Rhs) -> SourceExpr {
    match value {
        k::Rhs::Expr(value) => expression(value),
        k::Rhs::Action(value) => action(value),
    }
}
fn callee(value: &k::Callee) -> SourceExpr {
    match value {
        k::Callee::Declared(name) => call("declared", vec![text(name.as_str())]),
        k::Callee::Value(name) => call("value", vec![text(name.as_str())]),
        k::Callee::Library(id) => call("library", vec![text(id.to_string())]),
    }
}
fn action(value: &k::Action) -> SourceExpr {
    match value {
        k::Action::Call {
            callee: value,
            args,
        } => call(
            "call",
            vec![
                callee(value),
                SourceExpr::Array(args.iter().map(atom).collect()),
            ],
        ),
        k::Action::Spawn {
            callee: value,
            args,
        } => call(
            "spawn",
            vec![
                callee(value),
                SourceExpr::Array(args.iter().map(atom).collect()),
            ],
        ),
        k::Action::Perform {
            effect,
            args,
            result,
        } => call(
            "perform",
            vec![
                text(effect.as_str()),
                SourceExpr::Array(args.iter().map(atom).collect()),
                type_expr(result),
            ],
        ),
        k::Action::Sleep { duration } => call("sleep", vec![atom(duration)]),
        k::Action::Join { task } => call("join", vec![atom(task)]),
        k::Action::JoinMany { mode, tasks } => call(
            "joinMany",
            vec![
                text(match mode {
                    k::JoinMode::All => "all",
                    k::JoinMode::AllSettled => "all_settled",
                    k::JoinMode::Race => "race",
                    k::JoinMode::Any => "any",
                }),
                atom(tasks),
            ],
        ),
        k::Action::Yield => call("yield", vec![]),
        k::Action::Cancel { task } => call("cancel", vec![atom(task)]),
    }
}
fn type_expr(value: &k::Type) -> SourceExpr {
    // Type serialization has no fallible/custom serializer.
    call(
        "type",
        vec![text(
            serde_json::to_string(value).unwrap_or_else(|_| unreachable!("kernel types serialize")),
        )],
    )
}
fn atom(value: &k::Atom) -> SourceExpr {
    match value {
        k::Atom::Variable(name) => SourceExpr::Variable(name.clone()),
        k::Atom::Literal(value) => literal(value),
    }
}
fn literal(value: &k::Literal) -> SourceExpr {
    match value {
        k::Literal::Null => SourceExpr::Null,
        k::Literal::Absent => call("absent", vec![]),
        k::Literal::Bool(value) => SourceExpr::Bool(*value),
        k::Literal::Int(value) => call("int", vec![text(value.to_string())]),
        k::Literal::Float(value) => call("float", vec![text(value.to_string())]),
        k::Literal::Text(value) => text(value.clone()),
        k::Literal::Bytes(value) => call("bytes", vec![text(value.to_hex())]),
        k::Literal::Function(name) => call("function", vec![text(name.as_str())]),
    }
}
fn member(value: &k::Member) -> SourceExpr {
    match value {
        k::Member::Field { target, field } => {
            call("field", vec![expression(target), text(field.clone())])
        }
        k::Member::Index { target, index } => {
            call("index", vec![expression(target), expression(index)])
        }
    }
}
fn expression(value: &k::Expr) -> SourceExpr {
    match value {
        k::Expr::Literal(value) => literal(value),
        k::Expr::Variable(name) => SourceExpr::Variable(name.clone()),
        k::Expr::Tuple(items) => call("tuple", items.iter().map(expression).collect()),
        k::Expr::List(items) => call("list", items.iter().map(expression).collect()),
        k::Expr::Set(items) => call("set", items.iter().map(expression).collect()),
        k::Expr::Map(items) => call(
            "map",
            items
                .iter()
                .map(|entry| {
                    SourceExpr::Array(vec![expression(&entry.key), expression(&entry.value)])
                })
                .collect(),
        ),
        k::Expr::Record(items) => call(
            "record",
            items
                .iter()
                .map(|entry| {
                    SourceExpr::Array(vec![text(entry.field.clone()), expression(&entry.value)])
                })
                .collect(),
        ),
        k::Expr::Member(value) => member(value),
        k::Expr::Closure(value) => function_expr(&value.params, &value.body),
        k::Expr::Call { function, args } => call(
            "invoke",
            vec![
                text(function.to_string()),
                SourceExpr::Array(args.iter().map(expression).collect()),
            ],
        ),
        k::Expr::Clock => call("clock", vec![]),
        k::Expr::Random => call("random", vec![]),
        k::Expr::Read(value) => call(
            "read",
            vec![expression(&value.handle), expression(&value.request)],
        ),
    }
}
