//! FIG-7237: bounded source fuzzing above the TypeScript erasure boundary.
//! Shared by the libFuzzer target and its deterministic corpus replay.

use lash_dialect_typescript::{DiagnosticCode, SourceLimits, TypeScript};
use lash_ext_helper_compiler::library::{Dialect, Library};
use lash_kernel_dialect::{CellEnvironment, Environment, FrontEnd, Mode, Namespaces};
use lash_kernel_doc::{parse_document, print_document, validate_document};

pub const MAX_INPUT: usize = 1024;
pub const MAX_SOURCE: usize = 4096;

/// A structured counterpart to arbitrary bytes. Each production puts syntax
/// on the erasure/adaptation boundary; mutations are allowed to break it.
pub fn generated(data: &[u8]) -> String {
    let n = data.get(1).copied().unwrap_or(0) % 16;
    match data.first().copied().unwrap_or(0) % 12 {
        0 => format!("type N = number; const x: N = {n}; x;"),
        1 => format!("function id<T>(x: T): T {{ return x; }} id<number>({n});"),
        2 => format!("const a = {n}; const b = 8; const c = 1; a < b > (c);"),
        3 => format!("enum E {{ A = {n}, B, C = 'text' }} E.B;"),
        4 => format!("const enum E {{ A = {n}, B }} E.B;"),
        5 => format!("const x = ({{v: {n}}} satisfies {{v: number}}); (x as {{v: number}})!.v;"),
        6 => format!("const x: {{v?: number}} | null = {{v: {n}}}; x?.v ?? 0;"),
        7 => format!("interface Box<T> {{ v: T }} const x: Box<number> = {{v: {n}}}; x.v;"),
        8 => format!("const id = <T,>(x: T): T => x; id<number>({n});"),
        9 => format!("type T = {{a: number}} & {{b?: string}}; const x = <T>{{a: {n}}}; x!.a;"),
        10 => format!("enum E {{ A = {n}, B = }} const x: = E.;"),
        _ => format!("function f<T extends {{v: number}}>(x: T {{ return x?.v as; }} f<{n}>("),
    }
}

/// Byte edits deliberately include truncation and broken Unicode/token edges.
/// Lossy decoding is the input boundary, not a repair of TypeScript syntax.
pub fn mutated(source: &str, data: &[u8]) -> String {
    const TOKENS: &[&[u8]] = &[
        b"<T>",
        b" > (c)",
        b": number",
        b"enum E{A=",
        b"const enum",
        b" satisfies ",
        b" as ",
        b"!",
        b"?.",
        b"/*",
        b"\"",
        b"\x00",
        b"\xff",
        "é".as_bytes(),
    ];
    let mut bytes = source.as_bytes().to_vec();
    for edit in data.chunks(3).take(6) {
        let at = usize::from(edit[0]) % (bytes.len() + 1);
        match edit.get(1).copied().unwrap_or(0) % 4 {
            0 => bytes.truncate(at),
            1 if at < bytes.len() => {
                bytes.remove(at);
            }
            2 if at < bytes.len() => bytes[at] = edit.get(2).copied().unwrap_or(0),
            _ => {
                let token = TOKENS[usize::from(edit.get(2).copied().unwrap_or(0)) % TOKENS.len()];
                bytes.splice(at..at, token.iter().copied());
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[derive(Default, Debug)]
pub struct Counts {
    pub sources: usize,
    pub lowered: usize,
    pub admitted: usize,
    pub source_refusals: usize,
    pub admission_refusals: usize,
}

pub struct Harness {
    library: Library,
    parser: lash_dialect_typescript::Parser,
    frontend: TypeScript,
    pub counts: Counts,
}

impl Harness {
    pub fn new() -> Result<Self, String> {
        let limits = SourceLimits::new(MAX_SOURCE, 16).map_err(|error| error.to_string())?;
        Ok(Self {
            library: Library::of(Dialect::TypeScript)?,
            parser: lash_dialect_typescript::Parser::default().with_limits(limits),
            frontend: TypeScript { limits },
            counts: Counts::default(),
        })
    }

    /// Never execute arbitrary admitted input: it can loop, read capabilities,
    /// or have semantics for which the erasure differential has no oracle.
    pub fn check(&mut self, data: &[u8]) {
        let data = &data[..data.len().min(MAX_INPUT)];
        let raw = String::from_utf8_lossy(data);
        let generated = generated(data);
        let mutated = mutated(&generated, data);
        for source in [raw.as_ref(), &generated, &mutated] {
            self.source(source, false);
            self.source(
                &format!("export async function main(): any {{ {source} }}"),
                true,
            );
        }
    }

    fn source(&mut self, source: &str, module: bool) {
        assert!(source.len() <= MAX_SOURCE);
        self.counts.sources += 1;
        let namespaces = Namespaces::default();
        let environment = Environment::new(&self.library.names, &namespaces);
        let mode = if module {
            Mode::Module
        } else {
            Mode::Cell(CellEnvironment::first())
        };
        let lowered = if module {
            self.frontend.lower(source, &environment, mode)
        } else {
            self.parser
                .lower(source, &environment, mode)
                .map_err(|diagnostic| {
                    // Assert the concrete code and all typed facts survive the
                    // same conversion the FrontEnd boundary uses; no prose parsing.
                    let code: DiagnosticCode = diagnostic.code;
                    let expected = diagnostic.clone();
                    let boundary = lash_kernel_dialect::Diagnostic::from(diagnostic);
                    assert_eq!(boundary.code, code.as_str());
                    assert_eq!(boundary.kind, expected.kind);
                    assert_eq!(boundary.span, expected.span);
                    assert_eq!(boundary.repairs, expected.suggestions);
                    assert_eq!(boundary.operand_type, expected.operand_type);
                    assert_eq!(boundary.fact, expected.fact);
                    boundary
                })
        };
        let lowered = match lowered {
            Ok(lowered) => lowered,
            Err(diagnostic) => {
                assert!(!diagnostic.code.is_empty());
                assert!(!diagnostic.message.is_empty());
                self.counts.source_refusals += 1;
                return;
            }
        };
        self.counts.lowered += 1;
        validate_document(&lowered.document, &self.library.names)
            .unwrap_or_else(|error| panic!("invalid lowered source {source:?}: {error}"));
        if let Some(map) = &lowered.source_map {
            map.validate(&lowered.document, source)
                .unwrap_or_else(|error| panic!("invalid source map for {source:?}: {error}"));
        }
        let serialized = print_document(&lowered.document);
        let restored = parse_document(&serialized)
            .unwrap_or_else(|error| panic!("cannot read lowered source {source:?}: {error}"));
        assert_eq!(&restored, lowered.document.document(), "source {source:?}");
        let admission = lash_kernel_check::Environment::new(&self.library.names);
        match lash_kernel_check::admit(&restored, &admission) {
            Ok(_) => self.counts.admitted += 1,
            Err(refusal) => {
                // Admission owns its typed reasons; a valid source can still
                // have an impossible operand type. Such a refusal is no crash.
                assert!(!refusal.errors.is_empty());
                self.counts.admission_refusals += 1;
            }
        }
    }
}
