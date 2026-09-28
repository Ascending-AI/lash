use super::*;

/// The atom that stands where a front end's name would: a module's identity
/// is its IR, whichever dialect lowered it, so two dialects that lower to the
/// same program share one module ref.
pub(super) const IR_ATOM: &str = "lashlang-ir";

/// A module's identity: the semantic generation, the host requirements, the
/// exports, and the complete span-free program with every name in it.
pub(super) fn module_ref(
    program: &Program,
    host_requirements_ref: &HostRequirementsRef,
    exports: &ModuleExports,
) -> ModuleRef {
    let mut writer = HashWriter::new();
    writer.atom(LASHLANG_SEMANTIC_HASH_VERSION);
    writer.atom("module");
    writer.atom(IR_ATOM);
    writer.atom(host_requirements_ref.as_str());
    write_exports(&mut writer, exports);
    write_program(&mut writer, program);
    ModuleRef::new(&writer.finish())
}
