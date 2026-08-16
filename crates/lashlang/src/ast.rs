use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::lexer::Span;

pub type AstString = CompactString;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Program {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub declarations: Vec<Declaration>,
    pub main: Expr,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub declaration_spans: Vec<Span>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expression_spans: Vec<Span>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expression_source_spans: Vec<ExpressionSourceSpan>,
}

impl Program {
    pub fn block(expressions: Vec<Expr>) -> Self {
        Self {
            declarations: Vec::new(),
            main: Expr::Block(expressions),
            declaration_spans: Vec::new(),
            expression_spans: Vec::new(),
            expression_source_spans: Vec::new(),
        }
    }

    pub(crate) fn module_with_spans(
        declarations: Vec<Declaration>,
        declaration_spans: Vec<Span>,
        expressions: Vec<Expr>,
        expression_spans: Vec<Span>,
        expression_source_spans: Vec<ExpressionSourceSpan>,
    ) -> Self {
        Self {
            declarations,
            main: Expr::Block(expressions),
            declaration_spans,
            expression_spans,
            expression_source_spans,
        }
    }

    pub fn process(&self, name: &str) -> Option<&ProcessDecl> {
        self.declarations
            .iter()
            .find_map(|declaration| match declaration {
                Declaration::Process(process) if process.name.as_str() == name => Some(process),
                _ => None,
            })
    }
}

impl PartialEq for Program {
    fn eq(&self, other: &Self) -> bool {
        self.declarations == other.declarations && self.main == other.main
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpressionSourceSpan {
    pub path: Vec<u32>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Declaration {
    Type(TypeDecl),
    Process(ProcessDecl),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TypeDecl {
    pub name: AstString,
    pub ty: TypeExpr,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessDecl {
    pub name: AstString,
    pub params: Vec<ProcessParam>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signals: Vec<ProcessSignalDecl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_ty: Option<TypeExpr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<LabelMetadata>,
    pub body: Expr,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessParam {
    pub name: AstString,
    pub ty: TypeExpr,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessSignalDecl {
    pub name: AstString,
    pub ty: TypeExpr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelMetadata {
    pub title: AstString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<AstString>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssignTarget {
    pub root: AstString,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<AssignPathStep>,
}

impl AssignTarget {
    pub fn variable(root: AstString) -> Self {
        Self {
            root,
            steps: Vec::new(),
        }
    }

    pub fn is_simple(&self) -> bool {
        self.steps.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AssignPathStep {
    Field(AstString),
    Index(Expr),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    Block(Vec<Expr>),
    LabelAnnotated {
        label: LabelMetadata,
        expr: Box<Expr>,
    },
    Null,
    Bool(bool),
    Number(f64),
    String(AstString),
    Variable(AstString),
    Tuple(Vec<Expr>),
    List(Vec<Expr>),
    ListComprehension {
        element: Box<Expr>,
        clauses: Vec<ListComprehensionClause>,
    },
    Record(Vec<(AstString, Expr)>),
    Assign {
        target: AssignTarget,
        expr: Box<Expr>,
    },
    If {
        condition: Box<Expr>,
        then_block: Box<Expr>,
        else_block: Box<Expr>,
    },
    For {
        binding: AstString,
        iterable: Box<Expr>,
        body: Box<Expr>,
    },
    While {
        condition: Box<Expr>,
        body: Box<Expr>,
    },
    Break,
    Continue,
    StartProcess(ProcessStartExpr),
    ProcessRef {
        process: AstString,
    },
    HostDescriptorConstructor {
        type_name: AstString,
        input: Box<Expr>,
    },
    ResourceRef(ResourceRefExpr),
    ReceiverCall {
        receiver: Box<Expr>,
        operation: AstString,
        args: Vec<Expr>,
    },
    Await(Box<Expr>),
    SleepFor(Box<Expr>),
    SleepUntil(Box<Expr>),
    WaitSignal {
        name: AstString,
    },
    SignalRun {
        run: Box<Expr>,
        name: AstString,
        payload: Box<Expr>,
    },
    ResultUnwrap(Box<Expr>),
    Cancel(Box<Expr>),
    Print(Box<Expr>),
    Yield(Box<Expr>),
    Wake(Box<Expr>),
    Finish(Box<Expr>),
    Fail(Box<Expr>),
    BuiltinCall {
        name: AstString,
        args: Vec<Expr>,
    },
    /// A user-defined function value. This node is AST-only; the parser never
    /// produces it. Captures are copied when the closure is created.
    Function(Box<FunctionExpr>),
    /// Calls a user-defined function value.
    Call {
        function: Box<Expr>,
        args: Vec<Expr>,
    },
    /// AST-only map intrinsic used to exercise builtin-to-VM callbacks.
    Map {
        items: Box<Expr>,
        function: Box<Expr>,
    },
    Field {
        target: Box<Expr>,
        field: AstString,
    },
    Index {
        target: Box<Expr>,
        index: Box<Expr>,
    },
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        left: Box<Expr>,
        op: BinaryOp,
        right: Box<Expr>,
    },
    TypeLiteral(Box<TypeExpr>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FunctionExpr {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<AstString>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<AstString>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub captures: Vec<AstString>,
    pub body: Box<Expr>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ListComprehensionClause {
    For { binding: AstString, iterable: Expr },
    If { condition: Expr },
}

impl Expr {
    /// Yields every direct child expression of `self` in evaluation order.
    ///
    /// This is the single structural-traversal primitive: any pass that only
    /// needs to recurse into the sub-expressions of a node (without caring
    /// about the node's own kind) can fold over `children()` instead of
    /// re-spelling the full `match`. Leaf nodes (`Null`, `Bool`, `Number`,
    /// `String`, `Variable`, `Break`, `Continue`, `WaitSignal`,
    /// `ResourceRef`, `ProcessRef`, `HostDescriptorConstructor` metadata, and
    /// `TypeLiteral`) yield nothing.
    ///
    /// `Assign` includes any dynamic index expressions in its `target` path
    /// (in path order) before the assigned value, matching the order in which
    /// the compiler and linker visit them.
    pub fn children(&self) -> ExprChildren<'_> {
        let mut buffer = SmallExprVec::new();
        match self {
            Expr::Null
            | Expr::Bool(_)
            | Expr::Number(_)
            | Expr::String(_)
            | Expr::Variable(_)
            | Expr::Break
            | Expr::Continue
            | Expr::WaitSignal { .. }
            | Expr::ProcessRef { .. }
            | Expr::ResourceRef(_)
            | Expr::TypeLiteral(_) => {}
            Expr::Block(expressions) | Expr::Tuple(expressions) | Expr::List(expressions) => {
                buffer.extend(expressions.iter());
            }
            Expr::ListComprehension { element, clauses } => {
                for clause in clauses {
                    match clause {
                        ListComprehensionClause::For { iterable, .. } => buffer.push(iterable),
                        ListComprehensionClause::If { condition } => buffer.push(condition),
                    }
                }
                buffer.push(element);
            }
            Expr::LabelAnnotated { expr, .. } => buffer.push(expr),
            Expr::Record(entries) => buffer.extend(entries.iter().map(|(_, value)| value)),
            Expr::Assign { target, expr } => {
                for step in &target.steps {
                    if let AssignPathStep::Index(index) = step {
                        buffer.push(index);
                    }
                }
                buffer.push(expr);
            }
            Expr::If {
                condition,
                then_block,
                else_block,
            } => {
                buffer.push(condition);
                buffer.push(then_block);
                buffer.push(else_block);
            }
            Expr::For { iterable, body, .. } => {
                buffer.push(iterable);
                buffer.push(body);
            }
            Expr::While { condition, body } => {
                buffer.push(condition);
                buffer.push(body);
            }
            Expr::StartProcess(start) => buffer.extend(start.args.iter().map(|(_, value)| value)),
            Expr::HostDescriptorConstructor { input, .. } => buffer.push(input),
            Expr::ReceiverCall { receiver, args, .. } => {
                buffer.push(receiver);
                buffer.extend(args.iter());
            }
            Expr::SignalRun { run, payload, .. } => {
                buffer.push(run);
                buffer.push(payload);
            }
            Expr::Await(expr)
            | Expr::SleepFor(expr)
            | Expr::SleepUntil(expr)
            | Expr::ResultUnwrap(expr)
            | Expr::Cancel(expr)
            | Expr::Print(expr)
            | Expr::Yield(expr)
            | Expr::Wake(expr)
            | Expr::Fail(expr)
            | Expr::Unary { expr, .. } => buffer.push(expr),
            Expr::Finish(expr) => buffer.push(expr),
            Expr::BuiltinCall { args, .. } => buffer.extend(args.iter()),
            Expr::Function(function) => buffer.push(&function.body),
            Expr::Call { function, args } => {
                buffer.push(function);
                buffer.extend(args.iter());
            }
            Expr::Map { items, function } => {
                buffer.push(items);
                buffer.push(function);
            }
            Expr::Field { target, .. } => buffer.push(target),
            Expr::Index { target, index } => {
                buffer.push(target);
                buffer.push(index);
            }
            Expr::Binary { left, right, .. } => {
                buffer.push(left);
                buffer.push(right);
            }
        }
        ExprChildren {
            buffer,
            position: 0,
        }
    }
}

type SmallExprVec<'expr> = smallvec::SmallVec<[&'expr Expr; 3]>;

/// Iterator over the direct child expressions yielded by [`Expr::children`].
pub struct ExprChildren<'expr> {
    buffer: SmallExprVec<'expr>,
    position: usize,
}

impl<'expr> Iterator for ExprChildren<'expr> {
    type Item = &'expr Expr;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.buffer.get(self.position).copied();
        if item.is_some() {
            self.position += 1;
        }
        item
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.buffer.len() - self.position;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for ExprChildren<'_> {}

pub trait ExprVisitor {
    fn visit_expr(&mut self, expr: &Expr) {
        walk_expr(self, expr);
    }
}

pub fn walk_expr<V>(visitor: &mut V, expr: &Expr)
where
    V: ExprVisitor + ?Sized,
{
    for child in expr.children() {
        visitor.visit_expr(child);
    }
}

pub trait ExprFolder {
    fn fold_expr(&mut self, expr: Expr) -> Expr {
        fold_expr_children(self, expr)
    }
}

pub fn fold_expr_children<F>(folder: &mut F, expr: Expr) -> Expr
where
    F: ExprFolder + ?Sized,
{
    match expr {
        Expr::Block(expressions) => Expr::Block(
            expressions
                .into_iter()
                .map(|expr| folder.fold_expr(expr))
                .collect(),
        ),
        Expr::LabelAnnotated { label, expr } => Expr::LabelAnnotated {
            label,
            expr: Box::new(folder.fold_expr(*expr)),
        },
        Expr::Tuple(items) => Expr::Tuple(
            items
                .into_iter()
                .map(|expr| folder.fold_expr(expr))
                .collect(),
        ),
        Expr::List(items) => Expr::List(
            items
                .into_iter()
                .map(|expr| folder.fold_expr(expr))
                .collect(),
        ),
        Expr::ListComprehension { element, clauses } => Expr::ListComprehension {
            element: Box::new(folder.fold_expr(*element)),
            clauses: clauses
                .into_iter()
                .map(|clause| fold_list_comprehension_clause(folder, clause))
                .collect(),
        },
        Expr::Record(entries) => Expr::Record(
            entries
                .into_iter()
                .map(|(name, value)| (name, folder.fold_expr(value)))
                .collect(),
        ),
        Expr::Assign { target, expr } => Expr::Assign {
            target: fold_assign_target(folder, target),
            expr: Box::new(folder.fold_expr(*expr)),
        },
        Expr::If {
            condition,
            then_block,
            else_block,
        } => Expr::If {
            condition: Box::new(folder.fold_expr(*condition)),
            then_block: Box::new(folder.fold_expr(*then_block)),
            else_block: Box::new(folder.fold_expr(*else_block)),
        },
        Expr::For {
            binding,
            iterable,
            body,
        } => Expr::For {
            binding,
            iterable: Box::new(folder.fold_expr(*iterable)),
            body: Box::new(folder.fold_expr(*body)),
        },
        Expr::While { condition, body } => Expr::While {
            condition: Box::new(folder.fold_expr(*condition)),
            body: Box::new(folder.fold_expr(*body)),
        },
        Expr::StartProcess(mut start) => {
            start.args = start
                .args
                .into_iter()
                .map(|(name, value)| (name, folder.fold_expr(value)))
                .collect();
            Expr::StartProcess(start)
        }
        Expr::ProcessRef { process } => Expr::ProcessRef { process },
        Expr::HostDescriptorConstructor { type_name, input } => Expr::HostDescriptorConstructor {
            type_name,
            input: Box::new(folder.fold_expr(*input)),
        },
        Expr::ReceiverCall {
            receiver,
            operation,
            args,
        } => Expr::ReceiverCall {
            receiver: Box::new(folder.fold_expr(*receiver)),
            operation,
            args: args
                .into_iter()
                .map(|expr| folder.fold_expr(expr))
                .collect(),
        },
        Expr::Await(expr) => Expr::Await(Box::new(folder.fold_expr(*expr))),
        Expr::SleepFor(expr) => Expr::SleepFor(Box::new(folder.fold_expr(*expr))),
        Expr::SleepUntil(expr) => Expr::SleepUntil(Box::new(folder.fold_expr(*expr))),
        Expr::SignalRun { run, name, payload } => Expr::SignalRun {
            run: Box::new(folder.fold_expr(*run)),
            name,
            payload: Box::new(folder.fold_expr(*payload)),
        },
        Expr::ResultUnwrap(expr) => Expr::ResultUnwrap(Box::new(folder.fold_expr(*expr))),
        Expr::Cancel(expr) => Expr::Cancel(Box::new(folder.fold_expr(*expr))),
        Expr::Print(expr) => Expr::Print(Box::new(folder.fold_expr(*expr))),
        Expr::Yield(expr) => Expr::Yield(Box::new(folder.fold_expr(*expr))),
        Expr::Wake(expr) => Expr::Wake(Box::new(folder.fold_expr(*expr))),
        Expr::Finish(expr) => Expr::Finish(Box::new(folder.fold_expr(*expr))),
        Expr::Fail(expr) => Expr::Fail(Box::new(folder.fold_expr(*expr))),
        Expr::BuiltinCall { name, args } => Expr::BuiltinCall {
            name,
            args: args
                .into_iter()
                .map(|expr| folder.fold_expr(expr))
                .collect(),
        },
        Expr::Function(function) => Expr::Function(Box::new(FunctionExpr {
            name: function.name,
            params: function.params,
            captures: function.captures,
            body: Box::new(folder.fold_expr(*function.body)),
        })),
        Expr::Call { function, args } => Expr::Call {
            function: Box::new(folder.fold_expr(*function)),
            args: args
                .into_iter()
                .map(|expr| folder.fold_expr(expr))
                .collect(),
        },
        Expr::Map { items, function } => Expr::Map {
            items: Box::new(folder.fold_expr(*items)),
            function: Box::new(folder.fold_expr(*function)),
        },
        Expr::Field { target, field } => Expr::Field {
            target: Box::new(folder.fold_expr(*target)),
            field,
        },
        Expr::Index { target, index } => Expr::Index {
            target: Box::new(folder.fold_expr(*target)),
            index: Box::new(folder.fold_expr(*index)),
        },
        Expr::Unary { op, expr } => Expr::Unary {
            op,
            expr: Box::new(folder.fold_expr(*expr)),
        },
        Expr::Binary { left, op, right } => Expr::Binary {
            left: Box::new(folder.fold_expr(*left)),
            op,
            right: Box::new(folder.fold_expr(*right)),
        },
        leaf @ (Expr::Null
        | Expr::Bool(_)
        | Expr::Number(_)
        | Expr::String(_)
        | Expr::Variable(_)
        | Expr::Break
        | Expr::Continue
        | Expr::ResourceRef(_)
        | Expr::WaitSignal { .. }
        | Expr::TypeLiteral(_)) => leaf,
    }
}

fn fold_list_comprehension_clause<F>(
    folder: &mut F,
    clause: ListComprehensionClause,
) -> ListComprehensionClause
where
    F: ExprFolder + ?Sized,
{
    match clause {
        ListComprehensionClause::For { binding, iterable } => ListComprehensionClause::For {
            binding,
            iterable: folder.fold_expr(iterable),
        },
        ListComprehensionClause::If { condition } => ListComprehensionClause::If {
            condition: folder.fold_expr(condition),
        },
    }
}

fn fold_assign_target<F>(folder: &mut F, target: AssignTarget) -> AssignTarget
where
    F: ExprFolder + ?Sized,
{
    AssignTarget {
        root: target.root,
        steps: target
            .steps
            .into_iter()
            .map(|step| match step {
                AssignPathStep::Field(field) => AssignPathStep::Field(field),
                AssignPathStep::Index(index) => AssignPathStep::Index(folder.fold_expr(index)),
            })
            .collect(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypeExpr {
    Any,
    Str,
    Int,
    Float,
    Bool,
    Dict,
    /// The literal `null` type; usually only useful as part of a
    /// `Union` (e.g. `str | null` for a nullable field).
    Null,
    Enum(Vec<AstString>),
    List(Box<TypeExpr>),
    Object(Vec<TypeField>),
    Ref(AstString),
    Process {
        input: Box<TypeExpr>,
        output: Box<TypeExpr>,
        input_count: usize,
    },
    TriggerHandle(Box<TypeExpr>),
    /// Union of alternative type shapes, e.g. `str | int | null`.
    /// Always has two or more variants; single-variant parses collapse
    /// to the underlying `TypeExpr` in the parser.
    Union(Vec<TypeExpr>),
}

pub fn format_type_expr(ty: &TypeExpr) -> String {
    match ty {
        TypeExpr::Any => "any".to_string(),
        TypeExpr::Str => "str".to_string(),
        TypeExpr::Int => "int".to_string(),
        TypeExpr::Float => "float".to_string(),
        TypeExpr::Bool => "bool".to_string(),
        TypeExpr::Dict => "dict".to_string(),
        TypeExpr::Null => "null".to_string(),
        TypeExpr::Enum(values) => format!(
            "enum[{}]",
            values
                .iter()
                .map(|value| format!("\"{value}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        TypeExpr::List(item) => format!("list[{}]", format_type_expr(item)),
        TypeExpr::Object(fields) => {
            let fields = fields
                .iter()
                .map(|field| {
                    let optional = if field.optional { "?" } else { "" };
                    format!(
                        "{}: {}{}",
                        field.name,
                        format_type_expr(&field.ty),
                        optional
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{ {fields} }}")
        }
        TypeExpr::Ref(name) => name.to_string(),
        TypeExpr::Process { input, output, .. } => {
            format!(
                "Process<{}, {}>",
                format_type_expr(input),
                format_type_expr(output)
            )
        }
        TypeExpr::TriggerHandle(event) => {
            format!("TriggerHandle<{}>", format_type_expr(event))
        }
        TypeExpr::Union(items) => items
            .iter()
            .map(format_type_expr)
            .collect::<Vec<_>>()
            .join(" | "),
    }
}

impl fmt::Display for TypeExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_type_expr(self))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeField {
    pub name: AstString,
    pub ty: TypeExpr,
    pub optional: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessStartExpr {
    pub process: AstString,
    pub args: Vec<(AstString, Expr)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceRefExpr {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<AstString>,
    pub resource_type: AstString,
    pub alias: AstString,
}

impl ResourceRefExpr {
    pub fn unresolved(path: Vec<AstString>) -> Self {
        Self {
            path,
            resource_type: AstString::default(),
            alias: AstString::default(),
        }
    }

    pub fn resolved(
        path: Vec<AstString>,
        resource_type: impl Into<AstString>,
        alias: impl Into<AstString>,
    ) -> Self {
        Self {
            path,
            resource_type: resource_type.into(),
            alias: alias.into(),
        }
    }

    pub fn path_string(&self) -> String {
        if self.path.is_empty() {
            format!("{}.{}", self.resource_type, self.alias)
        } else {
            self.path
                .iter()
                .map(AstString::as_str)
                .collect::<Vec<_>>()
                .join(".")
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnaryOp {
    Negate,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    In,
    And,
    Or,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_expr_formatting_covers_nested_shapes() {
        let ty = TypeExpr::Object(vec![
            TypeField {
                name: "status".into(),
                ty: TypeExpr::Enum(vec!["ok".into(), "err".into()]),
                optional: false,
            },
            TypeField {
                name: "tags".into(),
                ty: TypeExpr::List(Box::new(TypeExpr::Str)),
                optional: true,
            },
            TypeField {
                name: "owner".into(),
                ty: TypeExpr::Ref("User".into()),
                optional: false,
            },
            TypeField {
                name: "value".into(),
                ty: TypeExpr::Union(vec![TypeExpr::Int, TypeExpr::Null]),
                optional: false,
            },
        ]);

        assert_eq!(
            format_type_expr(&ty),
            r#"{ status: enum["ok", "err"], tags: list[str]?, owner: User, value: int | null }"#
        );
        assert_eq!(ty.to_string(), format_type_expr(&ty));
    }

    fn var(name: &str) -> Expr {
        Expr::Variable(name.into())
    }

    fn child_vars(expr: &Expr) -> Vec<String> {
        expr.children()
            .map(|child| match child {
                Expr::Variable(name) => name.to_string(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn children_yields_leaves_as_empty() {
        for leaf in [
            Expr::Null,
            Expr::Bool(true),
            Expr::Number(1.0),
            Expr::String("s".into()),
            var("x"),
            Expr::Break,
            Expr::Continue,
            Expr::WaitSignal {
                name: "ready".into(),
            },
            Expr::TypeLiteral(Box::new(TypeExpr::Str)),
        ] {
            let children: Vec<_> = leaf.children().collect();
            assert!(children.is_empty(), "{leaf:?} should have no children");
        }
    }

    #[test]
    fn children_yields_composite_subexpressions_in_order() {
        let block = Expr::Block(vec![var("a"), var("b"), var("c")]);
        assert_eq!(child_vars(&block), ["a", "b", "c"]);

        let record = Expr::Record(vec![("k1".into(), var("v1")), ("k2".into(), var("v2"))]);
        assert_eq!(child_vars(&record), ["v1", "v2"]);

        let if_expr = Expr::If {
            condition: Box::new(var("cond")),
            then_block: Box::new(var("then")),
            else_block: Box::new(var("else")),
        };
        assert_eq!(child_vars(&if_expr), ["cond", "then", "else"]);

        let while_expr = Expr::While {
            condition: Box::new(var("cond")),
            body: Box::new(var("body")),
        };
        assert_eq!(child_vars(&while_expr), ["cond", "body"]);

        let receiver = Expr::ReceiverCall {
            receiver: Box::new(var("recv")),
            operation: "op".into(),
            args: vec![var("arg0"), var("arg1")],
        };
        assert_eq!(child_vars(&receiver), ["recv", "arg0", "arg1"]);

        let binary = Expr::Binary {
            left: Box::new(var("left")),
            op: BinaryOp::Add,
            right: Box::new(var("right")),
        };
        assert_eq!(child_vars(&binary), ["left", "right"]);
    }

    #[test]
    fn children_yields_assign_index_steps_before_value() {
        let assign = Expr::Assign {
            target: AssignTarget {
                root: "root".into(),
                steps: vec![
                    AssignPathStep::Field("field".into()),
                    AssignPathStep::Index(var("idx")),
                ],
            },
            expr: Box::new(var("value")),
        };
        // Field steps contribute no child expressions; the dynamic index is
        // yielded before the assigned value.
        assert_eq!(child_vars(&assign), ["idx", "value"]);
    }

    #[test]
    fn children_handles_finish() {
        assert_eq!(child_vars(&Expr::Finish(Box::new(var("done")))), ["done"]);
    }

    #[test]
    fn children_size_hint_is_exact() {
        let block = Expr::Block(vec![var("a"), var("b"), var("c"), var("d")]);
        let iter = block.children();
        assert_eq!(iter.len(), 4);
        assert_eq!(iter.size_hint(), (4, Some(4)));
    }

    #[test]
    fn visitor_walks_descendants_through_single_child_boundary() {
        struct VariableCollector(Vec<String>);

        impl ExprVisitor for VariableCollector {
            fn visit_expr(&mut self, expr: &Expr) {
                if let Expr::Variable(name) = expr {
                    self.0.push(name.to_string());
                }
                walk_expr(self, expr);
            }
        }

        let expr = Expr::While {
            condition: Box::new(var("ready")),
            body: Box::new(Expr::Block(vec![
                Expr::Assign {
                    target: AssignTarget {
                        root: "items".into(),
                        steps: vec![AssignPathStep::Index(var("idx"))],
                    },
                    expr: Box::new(var("value")),
                },
                Expr::Finish(Box::new(var("done"))),
            ])),
        };

        let mut collector = VariableCollector(Vec::new());
        collector.visit_expr(&expr);

        assert_eq!(collector.0, ["ready", "idx", "value", "done"]);
    }

    #[test]
    fn folder_reconstructs_owned_expr_trees() {
        struct RenameVariables;

        impl ExprFolder for RenameVariables {
            fn fold_expr(&mut self, expr: Expr) -> Expr {
                match expr {
                    Expr::Variable(name) => Expr::Variable(format!("renamed_{name}").into()),
                    other => fold_expr_children(self, other),
                }
            }
        }

        let expr = Expr::Assign {
            target: AssignTarget {
                root: "items".into(),
                steps: vec![AssignPathStep::Index(var("idx"))],
            },
            expr: Box::new(Expr::List(vec![var("first"), var("second")])),
        };

        let mut folder = RenameVariables;
        let folded = folder.fold_expr(expr);

        let Expr::Assign { target, expr } = folded else {
            panic!("expected assign");
        };
        assert!(matches!(
            target.steps.as_slice(),
            [AssignPathStep::Index(Expr::Variable(name))] if name.as_str() == "renamed_idx"
        ));
        let Expr::List(items) = *expr else {
            panic!("expected list");
        };
        assert_eq!(items, vec![var("renamed_first"), var("renamed_second")]);
    }
}
