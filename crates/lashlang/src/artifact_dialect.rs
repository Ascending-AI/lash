use super::*;

/// The language atom every module identity is written with.
///
/// TypeScript is the only RLM dialect (ADR 0096), so this never varies. It is
/// still written, and written unconditionally, because it is part of the
/// identity of every TypeScript artifact already in a store: dropping it would
/// move every published module ref, which is a semantic-hash cutover this
/// change does not make and the arc explicitly does not want.
const LANGUAGE_ATOM: &str = "typescript";

pub(super) fn module_ref(
    program: &Program,
    host_requirements_ref: &HostRequirementsRef,
    exports: &ModuleExports,
) -> ModuleRef {
    let mut writer = HashWriter::new();
    writer.atom(LASHLANG_SEMANTIC_HASH_VERSION);
    writer.atom("module");
    writer.atom(LANGUAGE_ATOM);
    writer.atom(host_requirements_ref.as_str());
    write_exports(&mut writer, exports);
    write_program(&mut writer, program);
    ModuleRef::new(&writer.finish())
}
