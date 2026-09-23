//! Building blocks for programs that have no TypeScript spelling.
//!
//! ADR 0096 makes TypeScript the sole authored RLM dialect; lashlang names the
//! IR and the VM. A few of the facts these suites pin outlive the surface that
//! used to spell them — the `format` builtin's placeholder rules, the numeric
//! `range`/`ceil_div`/`floor_div` helpers, tuple values — because they are
//! properties of the IR rather than of any dialect. Those tests build the IR
//! directly instead of authoring source, so they keep pinning the VM without
//! naming a retired syntax.

use lashlang::{Expr, Program};

/// A program whose body is `expressions`, run in order.
pub fn program(expressions: Vec<Expr>) -> Program {
    Program::block(expressions)
}

/// A single-expression program that finishes with `expr`.
pub fn finish_program(expr: Expr) -> Program {
    program(vec![finish(expr)])
}

pub fn finish(expr: Expr) -> Expr {
    Expr::Finish(Box::new(expr))
}

pub fn call(name: &str, args: Vec<Expr>) -> Expr {
    Expr::BuiltinCall {
        name: name.into(),
        args,
    }
}

pub fn string(value: &str) -> Expr {
    Expr::String(value.into())
}

pub fn number(value: f64) -> Expr {
    Expr::Number(value)
}

pub fn list(items: Vec<Expr>) -> Expr {
    Expr::List(items)
}

pub fn var(name: &str) -> Expr {
    Expr::Variable(name.into())
}

pub fn assign(name: &str, expr: Expr) -> Expr {
    Expr::Assign {
        target: lashlang::AssignTarget::variable(name.into()),
        expr: Box::new(expr),
    }
}

/// `<root><steps> = <expr>`, for a write through a path.
pub fn assign_path(root: &str, steps: Vec<lashlang::AssignPathStep>, expr: Expr) -> Expr {
    Expr::Assign {
        target: lashlang::AssignTarget {
            root: root.into(),
            steps,
        },
        expr: Box::new(expr),
    }
}

pub fn field_step(name: &str) -> lashlang::AssignPathStep {
    lashlang::AssignPathStep::Field(name.into())
}

pub fn index_step(index: Expr) -> lashlang::AssignPathStep {
    lashlang::AssignPathStep::Index(index)
}

pub fn field(target: Expr, name: &str) -> Expr {
    Expr::Field {
        target: Box::new(target),
        field: name.into(),
    }
}

pub fn index(target: Expr, index: Expr) -> Expr {
    Expr::Index {
        target: Box::new(target),
        index: Box::new(index),
    }
}

pub fn add(left: Expr, right: Expr) -> Expr {
    Expr::Binary {
        op: lashlang::BinaryOp::Add,
        left: Box::new(left),
        right: Box::new(right),
    }
}

pub fn tuple(items: Vec<Expr>) -> Expr {
    Expr::Tuple(items)
}

pub fn record(fields: Vec<(&str, Expr)>) -> Expr {
    Expr::Record(
        fields
            .into_iter()
            .map(|(name, value)| (name.into(), value))
            .collect(),
    )
}

/// `for <binding> in range(0, <end>) { <body> }`
pub fn for_range(binding: &str, end: f64, body: Vec<Expr>) -> Expr {
    Expr::For {
        binding: binding.into(),
        iterable: Box::new(call("range", vec![number(0.0), number(end)])),
        bind: None,
        body: Box::new(Expr::Block(body)),
    }
}

/// `[<element> for <binding> in <iterable>]`
pub fn comprehension(element: Expr, binding: &str, iterable: Expr) -> Expr {
    Expr::ListComprehension {
        element: Box::new(element),
        clauses: vec![lashlang::ListComprehensionClause::For {
            binding: binding.into(),
            iterable,
        }],
    }
}
