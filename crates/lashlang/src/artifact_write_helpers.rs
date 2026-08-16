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

pub(super) fn write_unary_expr(
    writer: &mut HashWriter,
    tag: &'static str,
    expr: &Expr,
    normalizer: &NameNormalizer,
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
