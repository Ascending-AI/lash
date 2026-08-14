use super::*;

pub(super) fn is_lashlang_dialect(dialect: &crate::CompilationDialect) -> bool {
    *dialect == crate::CompilationDialect::Lashlang
}

pub(super) fn module_ref(
    program: &Program,
    host_requirements_ref: &HostRequirementsRef,
    exports: &ModuleExports,
    compilation_dialect: crate::CompilationDialect,
) -> ModuleRef {
    let mut writer = HashWriter::new();
    writer.atom(LASHLANG_SEMANTIC_HASH_VERSION);
    writer.atom("module");
    if compilation_dialect == crate::CompilationDialect::Typescript {
        writer.atom("typescript");
    }
    writer.atom(host_requirements_ref.as_str());
    write_exports(&mut writer, exports);
    write_program(&mut writer, program);
    ModuleRef::new(&writer.finish())
}
