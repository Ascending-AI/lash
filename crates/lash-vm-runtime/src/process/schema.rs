/// Re-expresses a lash type as the JSON Schema a tool contract carries.
///
/// The conversion lives in lash_vm beside its inverse so the two cannot
/// drift: a process handle is spelled in the `x-lash` keyword
/// rather than erased to `{}`, and what the exporter writes the importer reads
/// back as the same type.
pub fn lash_vm_type_expr_schema(ty: &lash_vm::TypeExpr) -> serde_json::Value {
    lash_vm::type_expr_to_json_schema(ty)
}
