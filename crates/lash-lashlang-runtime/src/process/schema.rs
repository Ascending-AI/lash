/// Restates a lash type as the JSON Schema a tool contract carries.
///
/// The conversion lives in lashlang beside its inverse so the two cannot
/// drift: a process or a trigger handle is spelled in the `x-lash` keyword
/// rather than erased to `{}`, and what the exporter writes the importer reads
/// back as the same type.
pub fn lashlang_type_expr_schema(ty: &lashlang::TypeExpr) -> serde_json::Value {
    lashlang::type_expr_to_json_schema(ty)
}
