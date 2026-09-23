use super::*;

pub(super) fn write_label_metadata(writer: &mut HashWriter, label: &LabelMetadata) {
    writer.atom("label");
    writer.atom(label.title.as_str());
    match &label.description {
        Some(description) => {
            writer.atom("description");
            writer.atom(description.as_str());
        }
        None => writer.atom("no-description"),
    }
}

pub(super) fn write_unary_expr<'program>(
    writer: &mut HashWriter,
    tag: &'static str,
    expr: &'program Expr,
    normalizer: &NameNormalizer<'program>,
) {
    writer.atom(tag);
    write_expr(writer, expr, normalizer);
}

pub(super) fn write_resource_ref(writer: &mut HashWriter, resource: &ResourceRefExpr) {
    writer.atom("path");
    writer.usize(resource.path.len());
    for segment in &resource.path {
        writer.atom(segment.as_str());
    }
    writer.atom("handle");
    writer.atom(resource.resource_type.as_str());
    writer.atom(resource.alias.as_str());
}

pub(super) fn write_unary_op(writer: &mut HashWriter, op: UnaryOp) {
    writer.atom(match op {
        UnaryOp::Negate => "negate",
        UnaryOp::Not => "not",
    });
}

pub(super) fn write_binary_op(writer: &mut HashWriter, op: BinaryOp) {
    writer.atom(match op {
        BinaryOp::Add => "add",
        BinaryOp::Subtract => "subtract",
        BinaryOp::Multiply => "multiply",
        BinaryOp::Divide => "divide",
        BinaryOp::Modulo => "modulo",
        BinaryOp::Equal => "equal",
        BinaryOp::NotEqual => "not-equal",
        BinaryOp::Less => "less",
        BinaryOp::LessEqual => "less-equal",
        BinaryOp::Greater => "greater",
        BinaryOp::GreaterEqual => "greater-equal",
        BinaryOp::In => "in",
        BinaryOp::And => "and",
        BinaryOp::Or => "or",
    });
}

pub(super) fn write_structural_role(writer: &mut HashWriter, role: &crate::ast::StructuralRole) {
    writer.atom(role.name());
    if let crate::ast::StructuralRole::CollectionTransform { operation } = role {
        writer.atom(operation.as_str());
    }
}

pub(super) fn write_process_origin(writer: &mut HashWriter, origin: &crate::ast::ProcessOrigin) {
    match origin {
        crate::ast::ProcessOrigin::Declared => writer.atom("origin-declared"),
        crate::ast::ProcessOrigin::Lifted { site } => {
            writer.atom("origin-lifted");
            match site.root {
                crate::ast::AstRoot::Main => writer.atom("main"),
                crate::ast::AstRoot::Declaration(index) => {
                    writer.atom("declaration");
                    writer.u32(index);
                }
            }
            writer.usize(site.steps.len());
            for step in &site.steps {
                writer.u32(*step);
            }
        }
    }
}
