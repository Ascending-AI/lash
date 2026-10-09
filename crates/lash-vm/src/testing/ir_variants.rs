//! The names of the IR's variants, for coverage laws.
//!
//! A law that claims to hold over every construct of the IR (the document's
//! total-editability law, the generated-program laws) walks its programs with
//! [`variants_in`] and compares the result with [`EXPR_VARIANT_NAMES`], so the
//! claim is checked and not asserted.

use std::collections::BTreeSet;

use crate::{Declaration, Expr, ExprSlot, ExprSlotVisitor, Program, walk_expr_slots};

/// The name of `expr`'s variant. The match has no wildcard arm, so a new
/// variant does not compile until it is named here, and every coverage law
/// that compares what it saw with [`EXPR_VARIANT_NAMES`] then fails until a
/// program of its corpus holds the variant.
pub fn expr_variant_name(expr: &Expr) -> &'static str {
    match expr {
        Expr::Block(_) => "Block",
        Expr::LabelAnnotated { .. } => "LabelAnnotated",
        Expr::Null => "Null",
        Expr::Absent => "Absent",
        Expr::Bool(_) => "Bool",
        Expr::Number(_) => "Number",
        Expr::String(_) => "String",
        Expr::Variable(_) => "Variable",
        Expr::List(_) => "List",
        Expr::Record(_) => "Record",
        Expr::Assign { .. } => "Assign",
        Expr::If { .. } => "If",
        Expr::For { .. } => "For",
        Expr::While { .. } => "While",
        Expr::Role { .. } => "Role",
        Expr::Break => "Break",
        Expr::Continue => "Continue",
        Expr::ProcessRef { .. } => "ProcessRef",
        Expr::HostDescriptorConstructor { .. } => "HostDescriptorConstructor",
        Expr::ResourceRef(_) => "ResourceRef",
        Expr::ReceiverCall { .. } => "ReceiverCall",
        Expr::Await(_) => "Await",
        Expr::SleepFor(_) => "SleepFor",
        Expr::ResultUnwrap(_) => "ResultUnwrap",
        Expr::Print(_) => "Print",
        Expr::Finish(_) => "Finish",
        Expr::Fail(_) => "Fail",
        Expr::BuiltinCall { .. } => "BuiltinCall",
        Expr::Function(_) => "Function",
        Expr::ProcessLiteral(_) => "ProcessLiteral",
        Expr::Call { .. } => "Call",
        Expr::MethodCall { .. } => "MethodCall",
        Expr::ThisCall { .. } => "ThisCall",
        Expr::FunctionCall { .. } => "FunctionCall",
        Expr::Map { .. } => "Map",
        Expr::Try(_) => "Try",
        Expr::Throw(_) => "Throw",
        Expr::FunctionReturn(_) => "FunctionReturn",
        Expr::Field { .. } => "Field",
        Expr::Index { .. } => "Index",
        Expr::CoercingUnary { .. } => "CoercingUnary",
        Expr::CoercingBinary { .. } => "CoercingBinary",
        Expr::OperandLogical { .. } => "OperandLogical",
    }
}

/// The names [`expr_variant_name`] answers, one per [`Expr`] variant.
pub const EXPR_VARIANT_NAMES: [&str; 43] = [
    "Block",
    "LabelAnnotated",
    "Null",
    "Absent",
    "Bool",
    "Number",
    "String",
    "Variable",
    "List",
    "Record",
    "Assign",
    "If",
    "For",
    "While",
    "Role",
    "Break",
    "Continue",
    "ProcessRef",
    "HostDescriptorConstructor",
    "ResourceRef",
    "ReceiverCall",
    "Await",
    "SleepFor",
    "ResultUnwrap",
    "Print",
    "Finish",
    "Fail",
    "BuiltinCall",
    "Function",
    "ProcessLiteral",
    "Call",
    "MethodCall",
    "ThisCall",
    "FunctionCall",
    "Map",
    "Try",
    "Throw",
    "FunctionReturn",
    "Field",
    "Index",
    "CoercingUnary",
    "CoercingBinary",
    "OperandLogical",
];

/// The body of `main` and of every declaration of `program`.
pub fn program_bodies(program: &Program) -> Vec<&Expr> {
    let mut bodies = vec![&program.main];
    for declaration in &program.declarations {
        bodies.push(match declaration {
            Declaration::Process(process) => &process.body,
            Declaration::Function(function) => &function.body,
        });
    }
    bodies
}

/// The name of every variant `program` holds below the root of a body, read
/// through the exhaustive slot walk.
pub fn variants_in(program: &Program) -> BTreeSet<&'static str> {
    struct Seen(BTreeSet<&'static str>);
    impl ExprSlotVisitor for Seen {
        fn visit_slot(&mut self, _path: &[ExprSlot], expr: &Expr) {
            self.0.insert(expr_variant_name(expr));
        }
    }
    let mut seen = Seen(BTreeSet::new());
    for body in program_bodies(program) {
        walk_expr_slots(&mut seen, body);
    }
    seen.0
}
