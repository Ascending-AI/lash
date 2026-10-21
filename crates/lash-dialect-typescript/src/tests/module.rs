//! MODULE-CONTRACT (FIG-6380): a TypeScript workflow module under the
//! workflow contract every dialect shares. These cases pin what each
//! TypeScript annotation means; the twin cases that compare entry names and
//! refusal codes with Python run through the host's import.

use std::collections::BTreeMap;

use lash_kernel_dialect::workflow::ModuleRefusal;
use lash_kernel_dialect::{Environment, Lowered, Mode};
use lash_vocabulary::{EffectName, Name, Param, RecordType, RecordTypeField, Signature, Type};

use crate::DiagnosticCode;

fn module(source: &str) -> Result<Lowered, crate::Diagnostic> {
    let effects = super::machine::effects();
    let environment_namespaces = crate::tests::linked::namespaces(&effects, super::controls())
        .expect("the test's effects are operations");
    let environment = Environment::new(super::library(), &environment_namespaces);
    let lowered = crate::lower(source, &environment, Mode::Module)?;
    lash_kernel_doc::validate_document(&lowered.document, super::library())
        .unwrap_or_else(|invalid| panic!("{invalid}"));
    lowered
        .source_map
        .as_ref()
        .expect("a module has its source map")
        .validate(&lowered.document, source)
        .unwrap_or_else(|refused| panic!("{refused}"));
    Ok(lowered)
}

fn entries(source: &str) -> BTreeMap<Name, Signature> {
    module(source)
        .unwrap_or_else(|diagnostic| panic!("{diagnostic}\n{source}"))
        .document
        .exports
        .iter()
        .filter_map(|(name, export)| match export {
            lash_kernel_doc::Export::Function(f) => Some((name.clone(), f.signature.clone())),
            _ => None,
        })
        .collect()
}

/// FIG-6507: a list-valued tool result admits as an array callback receiver.
#[test]
fn filtering_records_then_mapping_the_result_admits() {
    let mut effects = super::machine::effects();
    effects
        .get_mut(&EffectName::new("host.echo").unwrap())
        .unwrap()
        .result = Type::List(Box::new(Type::Any));
    let environment_namespaces = crate::tests::linked::namespaces(&effects, super::controls())
        .expect("the test's effects are operations");
    let environment = Environment::new(super::library(), &environment_namespaces);
    let lowered = crate::lower(
        r#"
export async function unread_subjects() {
  const emails = await host.echo(null);
  const unread = emails.filter((email) => email.unread);
  const subjects = unread.map((email) => email.subject);
  return subjects;
}
"#,
        &environment,
        Mode::Module,
    )
    .unwrap();
    lash_kernel_check::derive(&lowered.document, super::library()).unwrap_or_else(|error| {
        panic!(
            "{error}\n{}",
            lash_kernel_doc::print_document(&lowered.document)
        );
    });
}

fn refusal(source: &str) -> DiagnosticCode {
    module(source).map_or_else(
        |diagnostic| diagnostic.code,
        |_| panic!("the module is refused:\n{source}"),
    )
}

fn param(name: &str, ty: Type) -> Param {
    Param {
        name: Name::new(name),
        ty,
        optional: false,
    }
}

/// Each annotation's kernel meaning in TypeScript: primitives, arrays,
/// object literals with optional fields, string-literal unions, `null`,
/// `undefined` as an optional parameter, named types as any value, and a
/// `Promise` result as what it resolves to.
#[test]
fn an_entry_signature_is_the_kernel_meaning_of_its_typescript_annotations() {
    let source = r#"
type Topic = { title: string };
export async function digest(
  inbox: string,
  count: number,
  flags: boolean[],
  topic: { title: string; urgent?: boolean },
  mode: "fast" | "slow" | null,
  named: Topic,
  limit?: number,
): Promise<string[]> {
  return [inbox];
}
"#;
    assert_eq!(
        entries(source),
        BTreeMap::from([(
            Name::new("digest"),
            Signature {
                params: vec![
                    param("inbox", Type::Text),
                    param("count", Type::Number),
                    param("flags", Type::List(Box::new(Type::Bool))),
                    param(
                        "topic",
                        Type::Record(RecordType {
                            fields: vec![
                                RecordTypeField {
                                    name: "title".into(),
                                    ty: Type::Text,
                                    optional: false,
                                },
                                RecordTypeField {
                                    name: "urgent".into(),
                                    ty: Type::Bool,
                                    optional: true,
                                },
                            ],
                            rest: None,
                        }),
                    ),
                    param("mode", Type::Union(vec![Type::Text, Type::Null])),
                    param("named", Type::Any),
                    Param {
                        name: Name::new("limit"),
                        ty: Type::Union(vec![Type::Number, Type::Null, Type::Absent]),
                        optional: true,
                    },
                ],
                result: Type::List(Box::new(Type::Text)),
            },
        )])
    );
}

/// Every top-level async function is an entry, exported or not, by
/// declaration or by `const`, arrow or function expression. One declared
/// inside a block, or bound by `let`, is ordinary code of the module.
#[test]
fn top_level_async_declarations_are_the_entries_and_block_local_ones_are_not() {
    let source = r#"
export async function declared() { return 1; }
async function unexported() { return 2; }
export const arrow = async (n: number) => n * 2;
const expression = async function () { return 3; };
let reassignable = async () => 4;
if (reassignable) {
  async function inBlock() { return 5; }
  const alsoInBlock = async () => 6;
}
function helper() { return 7; }
"#;
    assert_eq!(
        entries(source).into_keys().collect::<Vec<_>>(),
        ["arrow", "declared", "expression", "unexported"].map(Name::new)
    );
}

/// `process(...)` is not workflow syntax: an entry is the async function
/// itself, and `process` is an unknown name.
#[test]
fn process_wrapping_is_not_an_entry_form() {
    assert_eq!(
        refusal("export const digest = process(async (inbox: string) => inbox);"),
        DiagnosticCode::UnknownBinding
    );
}

/// The contract's refusals, under the codes every dialect shares.
#[test]
fn the_contract_refuses_under_the_shared_codes() {
    for (source, expected) in [
        (
            "export async function a() { return 1; }\nexport async function a() { return 2; }",
            ModuleRefusal::DuplicateEntry,
        ),
        (
            "const a = async () => 1;\nexport async function a() { return 2; }",
            ModuleRefusal::DuplicateEntry,
        ),
        (
            "const limit = 3;\nexport async function a() { return limit; }",
            ModuleRefusal::CapturedModuleBinding,
        ),
        (
            "function helper() { return 1; }\nexport async function a() { return helper(); }",
            ModuleRefusal::CapturedModuleBinding,
        ),
        (
            "export async function b() { return 1; }\nexport async function a() { return await b(); }",
            ModuleRefusal::CapturedModuleBinding,
        ),
        (
            "export async function a(f: () => number) { return 1; }",
            ModuleRefusal::UnmappableEntryAnnotation,
        ),
        (
            "export async function a(): Promise<bigint> { return 1; }",
            ModuleRefusal::UnmappableEntryAnnotation,
        ),
        (
            "export async function a(limit: number = 3) { return limit; }",
            ModuleRefusal::EntryDefault,
        ),
    ] {
        assert_eq!(
            refusal(source),
            DiagnosticCode::Module(expected),
            "{source}"
        );
    }
}

/// ENTRY-CONV: host starts use JavaScript's arity and return boundary,
/// while the entry executes the author's function body.
#[test]
fn entry_conventions_bind_source_arguments_and_normalize_absent() {
    for (source, args, expected) in [
        (
            "export async function run(x) { return x; }",
            vec![
                lash_kernel_doc::Datum::Text("first".into()),
                lash_kernel_doc::Datum::Null,
            ],
            lash_kernel_doc::Datum::Text("first".into()),
        ),
        (
            "export async function run(x) { return x; }",
            vec![],
            lash_kernel_doc::Datum::Null,
        ),
        (
            "export async function run() { return undefined; }",
            vec![],
            lash_kernel_doc::Datum::Null,
        ),
        (
            "export async function run(x) { return [this === undefined, arguments.length, arguments[1], x]; }",
            vec![
                lash_kernel_doc::Datum::Null,
                lash_kernel_doc::Datum::Text("extra".into()),
            ],
            lash_kernel_doc::Datum::List(vec![
                lash_kernel_doc::Datum::Bool(true),
                lash_kernel_doc::Datum::Float(lash_kernel_doc::Float::new(2.0)),
                lash_kernel_doc::Datum::Text("extra".into()),
                lash_kernel_doc::Datum::Null,
            ]),
        ),
    ] {
        let document = module(source).unwrap().document.into_document();
        let entry = super::linked::exported(&document, &lash_vocabulary::Name::new("run"));
        assert_eq!(
            matches!(
                entry.convention,
                lash_kernel_doc::StartConvention::TsArgsList { .. }
            ),
            source.contains("arguments")
        );
        if matches!(
            entry.convention,
            lash_kernel_doc::StartConvention::TsDirect { .. }
        ) {
            assert_eq!(
                document.functions[&entry.function]
                    .signature
                    .params
                    .iter()
                    .map(|param| param.name.clone())
                    .collect::<Vec<_>>(),
                entry
                    .signature
                    .params
                    .iter()
                    .map(|param| param.name.clone())
                    .collect::<Vec<_>>()
            );
        }
        let (_, result) = super::machine::end_of_entry(document, "run", args);
        assert_eq!(result, expected, "{source}");
    }
}

/// K-ADM-007: a function without a start signature does not add the
/// dependencies of an entry adapter it cannot carry.
#[test]
fn a_function_without_a_start_signature_adds_no_entry_adapter_dependencies() {
    let lowered = super::lower("function f(x = 1) { return x; } f();").unwrap();
    lash_kernel_check::derive(&lowered.document, super::library()).unwrap_or_else(|error| {
        panic!(
            "{error}\n{}",
            lash_kernel_doc::print_document(&lowered.document)
        );
    });
}

/// Async returns adopt a returned promise after source finalizers complete.
#[test]
fn an_async_return_adopts_a_promise_as_an_entry_and_as_a_nested_call() {
    let source = "async function run() { async function child() { return 7; } return child(); }";
    let document = module(&format!("export {source}"))
        .unwrap()
        .document
        .into_document();
    let (_, result) = super::machine::end_of_entry(document, "run", vec![]);
    assert_eq!(
        result,
        lash_kernel_doc::Datum::Float(lash_kernel_doc::Float::new(7.0))
    );
    let recorded = super::machine::run(&format!("{source} console.log(await run());"), &[]);
    assert_eq!(recorded.end, "ok");
    assert_eq!(recorded.lines(), ["7"]);
}
