use super::*;

/// A module's identity: the semantic generation, the front end the program
/// was lowered from, the host requirements, the exports, and the complete
/// span-free program with every name in it.
pub(super) fn module_ref(
    program: &Program,
    host_requirements_ref: &HostRequirementsRef,
    exports: &ModuleExports,
) -> ModuleRef {
    let mut writer = HashWriter::new();
    writer.atom(LASHLANG_SEMANTIC_HASH_VERSION);
    writer.atom("module");
    writer.atom(program.language.as_str());
    writer.atom(host_requirements_ref.as_str());
    write_exports(&mut writer, exports);
    write_program(&mut writer, program);
    ModuleRef::new(&writer.finish())
}
