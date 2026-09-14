use super::*;

/// The language atom every module identity is written with.
///
/// TypeScript is the only RLM dialect (ADR 0096), so this never varies. It is
/// still written, and written unconditionally, to keep the hash input's shape
/// stable rather than to hold module refs still: `LASHLANG_SEMANTIC_HASH_VERSION`
/// is the first atom of the same hash, and this change moves it from v8 to v9,
/// so every published module ref moves with it regardless. Dropping the atom
/// would only make the input harder to read against the artifacts it names.
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
