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
export async function unread_subjects(): any {
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

/// Top-level function declarations and const functions are Exports, whether
/// synchronous or async. Block locals and mutable bindings stay ordinary code;
/// the Export retains the source's async flag for its settled public ABI.
#[test]
fn top_level_function_exports_keep_async_flags_and_exclude_block_locals() {
    let source = r#"
export async function declared(): any { return 1; }
async function unexported(): any { return 2; }
export const arrow = async (n: number): number => n * 2;
const expression = async function (): any { return 3; };
let reassignable = async (): any => 4;
if (reassignable) {
  async function inBlock(): any { return 5; }
  const alsoInBlock = async (): any => 6;
}
function helper(): any { return 7; }
"#;
    let lowered = module(source).unwrap();
    let functions = lowered
        .document
        .exports
        .iter()
        .filter_map(|(name, export)| match export {
            lash_kernel_doc::Export::Function(function) => {
                Some((name.clone(), function.asynchronous))
            }
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        functions,
        BTreeMap::from([
            (Name::new("arrow"), true),
            (Name::new("declared"), true),
            (Name::new("expression"), true),
            (Name::new("helper"), false),
            (Name::new("unexported"), true),
        ])
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
    // FIG-7135: a code-only module still has no host-startable entry.
    for source in ["const value = 3;", ""] {
        let diagnostic = module(source).expect_err("a module must declare an entry");
        assert_eq!(diagnostic.code.as_str(), "WORKFLOW_EMPTY_MODULE");
        assert_eq!(diagnostic.span, None);
        assert_eq!(
            diagnostic.kind,
            lash_kernel_dialect::DiagnosticKind::Refusal
        );
    }
    for (source, expected) in [
        (
            "export async function a(): any { return 1; }\nexport async function a(): any { return 2; }",
            ModuleRefusal::DuplicateEntry,
        ),
        (
            "const a = async (): any => 1;\nexport async function a(): any { return 2; }",
            ModuleRefusal::DuplicateEntry,
        ),
        (
            "const limit = 3;\nexport async function a(): any { return limit; }",
            ModuleRefusal::CapturedModuleBinding,
        ),
        (
            "function helper(): any { return 1; }\nexport async function a(): any { return helper(); }",
            ModuleRefusal::CapturedModuleBinding,
        ),
        (
            "export async function b(): any { return 1; }\nexport async function a(): any { return await b(); }",
            ModuleRefusal::CapturedModuleBinding,
        ),
        (
            "export async function a(limit: number = 3): any { return limit; }",
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
fn complete_entry_conventions_bind_public_arguments_and_normalize_absent() {
    for (source, args, expected) in [
        (
            "export async function run(x: any): any { return x; }",
            vec![lash_kernel_doc::Datum::Text("first".into())],
            lash_kernel_doc::Datum::Text("first".into()),
        ),
        (
            "export async function run(x?: any): any { return x; }",
            vec![],
            lash_kernel_doc::Datum::Null,
        ),
        (
            "export async function run(): any { return undefined; }",
            vec![],
            lash_kernel_doc::Datum::Null,
        ),
        (
            "export async function run(x: any, extra: any): any { return [this === undefined, arguments.length, arguments[1], x]; }",
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
        let entry =
            super::linked_inspection::exported(&document, &lash_vocabulary::Name::new("run"));
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

/// Async returns adopt a returned promise after source finalizers complete.
#[test]
fn an_async_return_adopts_a_promise_as_an_entry_and_as_a_nested_call() {
    let source =
        "async function run(): any { async function child(): any { return 7; } return child(); }";
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

fn selected_operations(source: &str, entry: &str) -> Vec<String> {
    let lowered = module(source).expect("reach witness lowers");
    let (content, root) = super::linked::content(&lowered.document);
    let reach = content
        .inspect(
            root,
            &lash_kernel_package::ReachSelection::Function(lash_vocabulary::ExportName {
                name: Name::new(entry),
                member: None,
            }),
            super::library(),
        )
        .expect("selected Link reach");
    reach
        .operations
        .values()
        .map(|need| match need {
            lash_kernel_package::OperationNeed::Fixed { name, .. }
            | lash_kernel_package::OperationNeed::Deferred { name, .. } => {
                format!("{}.{}", name.package, name.member)
            }
        })
        .collect()
}

/// FIG-7136: numeric indexing cannot manufacture built-in callable members.
/// The retained Link DAG may hold services that this Export does not reach.
#[test]
fn a_pure_indexed_factorial_reaches_no_read_imports() {
    for source in [
        r#"
export function factorial(n: number): number {
  const values = [1];
  for (let i = 1; i <= n; i++) { values[i] = values[i - 1] * i; }
  return values[n];
}
"#,
        r#"
export function factorial(n: number): number {
  const values = {0: 1};
  for (let i = 1; i <= n; i++) { values[i] = values[i - 1] * i; }
  return values[n];
}
"#,
    ] {
        let operations = selected_operations(source, "factorial");
        assert!(
            operations.is_empty(),
            "pure factorial reaches {operations:?}"
        );
        let cell = format!(
            "{} await control.finish(factorial(6));",
            source.replace("export ", "")
        );
        assert_eq!(
            super::machine::end(&cell),
            super::machine::Ended::Finished(lash_kernel_doc::Datum::Float(
                lash_kernel_doc::Float::new(720.0)
            )),
        );
    }
}

/// FIG-7136: a key that may name a service member must keep its operation
/// in the selected reach, even before the function is called.
#[test]
fn computed_service_members_keep_their_read_imports() {
    // The receiver comes from the caller. Spelling Date or Math here would
    // introduce their callable tokens and could conceal a missing member edge.
    let operations = selected_operations(
        "export function read(receiver: any, key: string): any { return receiver[key](); }",
        "read",
    );
    for operation in ["typescript.clock", "typescript.random"] {
        assert!(
            operations.iter().any(|name| name == operation),
            "computed member lost {operation}: {operations:?}"
        );
    }
}

/// FIG-7136 and readable-first source: a numeric record read keeps an ordinary
/// TypeScript spelling that re-lowers to the same cell document.
#[test]
fn numeric_indexing_exports_as_ordinary_typescript() {
    let lowered = super::lower("const values = {0: 7}; const key = 0; values[key];").unwrap();
    crate::print(
        &lowered.document,
        None,
        super::library(),
        Default::default(),
    )
    .expect("numeric indexing has a verified authored-source representation");
}

/// FIG-7109: the sleep duration conversion belongs to lowering, not source.
#[test]
fn sleep_prints_its_authored_duration_and_preserves_the_document() {
    for source in [
        "export async function demo(): Promise<string> { await sleep(5); return 'awake'; }",
        "export async function demo(delay: number): Promise<string> { await sleep(delay); return 'awake'; }",
    ] {
        let lowered = module(source).unwrap();
        let printed = crate::print(
            &lowered.document,
            Some(&lowered.captions),
            super::library(),
            Default::default(),
        )
        .expect("sleep has an exact authored inverse");
        let (read, captions) = crate::read(
            &printed.source,
            &lowered.document,
            super::library(),
            Default::default(),
            None,
        )
        .unwrap();
        assert_eq!(read.canonical_bytes(), lowered.document.canonical_bytes());
        assert_eq!(captions.entries(), lowered.captions.entries());
    }
}

/// FIG-7109: a failed callable lift is a fact about its exact code region.
#[test]
fn an_unprintable_export_refuses_with_its_callable_site() {
    use lash_kernel_doc::{Export, StartConvention};
    use lash_vocabulary::sites::{Site, Unit};
    let lowered = module("export function hidden(): number { return 1; }").unwrap();
    let mut document = lowered.document.document().clone();
    let Export::Function(export) = document.exports.get_mut(&Name::new("hidden")).unwrap() else {
        panic!("function export");
    };
    export.convention = StartConvention::Direct;
    let site = Site::new(Unit::Library(export.function), Vec::new());
    assert_eq!(
        crate::print(&document, None, super::library(), Default::default()),
        Err(lash_kernel_dialect::PrintError::Unprintable { site }),
        "the failed lift must survive instead of printing an empty module",
    );
}

/// FIG-7139: admission sees the typed operand before implicit conversion erases it.
#[test]
fn implicit_record_text_is_refused_at_its_operand() {
    for expression in [
        "`answered by ${event.answered_by}`",
        "'answered by ' + event.answered_by",
    ] {
        let source = format!(
            "export function answer(event: {{ answered_by: {{ identity_id: string; kind: string }} }}): any {{ return {expression}; }}"
        );
        let Err(diagnostic) = module(&source) else {
            panic!("implicit record text must be refused");
        };
        assert_eq!(diagnostic.code.as_str(), "TS_IMPLICIT_TEXT_CONVERSION");
        let start = source.rfind("event.answered_by").unwrap();
        assert_eq!(
            diagnostic.span,
            Some(lash_kernel_doc::Span {
                start,
                end: start + "event.answered_by".len(),
            })
        );
        assert!(diagnostic.suggestions.is_empty());
    }
}

/// FIG-7139: admission policy distinguishes known structures from Any and permits explicit text.
#[test]
fn implicit_text_policy_admits_only_the_selected_static_operands() {
    use lash_kernel_dialect::SourceOperandType as Static;
    use lash_kernel_dialect::workflow::ImplicitTextConversionPolicy as Policy;
    let namespaces = Default::default();
    let environment = Environment::new(super::library(), &namespaces);
    let lower = |source: &str, policy| {
        crate::lower_with_limits(
            source,
            &environment,
            Mode::Module,
            crate::SourceLimits::standard().with_implicit_text_conversion(policy),
        )
    };
    for (parameters, expression, expected) in [
        ("event: any", "event.answered_by", Static::Any),
        (
            "event: { answered_by: { identity_id: string; kind: string } }",
            "event.answered_by",
            Static::Record(
                [
                    ("identity_id".to_owned(), Static::Text),
                    ("kind".to_owned(), Static::Text),
                ]
                .into(),
            ),
        ),
        (
            "values: string[]",
            "values",
            Static::List(Box::new(Static::Text)),
        ),
        ("", "[1, 2]", Static::Array),
        ("", "({ id: 1 })", Static::Object),
        (
            "",
            "(() => 1)",
            Static::Function {
                result: Box::new(Static::Any),
            },
        ),
        (
            "",
            "((() => 1) as (() => number))",
            Static::Function {
                result: Box::new(Static::Number),
            },
        ),
        ("", "new Map()", Static::Map),
        ("", "new Set()", Static::Set),
        ("", "new Array(2)", Static::Array),
        (
            "value: string | { id: string }",
            "value",
            Static::Union(vec![
                Static::Text,
                Static::Record([("id".to_owned(), Static::Text)].into()),
            ]),
        ),
    ] {
        for expression in [
            format!("`value ${{{expression}}}`"),
            format!("'value ' + {expression}"),
            format!("{expression} + ' value'"),
        ] {
            let source =
                format!("export function answer({parameters}): any {{ return {expression}; }}");
            assert!(lower(&source, Policy::Allow).is_ok(), "allow: {source}");
            for policy in [Policy::Refuse, Policy::Strict] {
                let result = lower(&source, policy);
                if policy == Policy::Refuse && expected == Static::Any {
                    assert!(result.is_ok(), "unconstrained: {source}");
                } else {
                    let diagnostic = result
                        .err()
                        .unwrap_or_else(|| panic!("must refuse {source}"));
                    assert_eq!(diagnostic.code, DiagnosticCode::ImplicitTextConversion);
                    assert_eq!(
                        diagnostic.operand_type,
                        Some(Box::new(expected.clone())),
                        "{source}"
                    );
                    assert!(diagnostic.suggestions.is_empty());
                }
            }
        }
    }
    for expression in [
        "event.answered_by.identity_id",
        "42",
        "true",
        "null",
        "undefined",
        "String(event)",
        "JSON.stringify(event)",
        "JSON.stringify(undefined)",
    ] {
        for expression in [
            format!("`value ${{{expression}}}`"),
            format!("'value ' + {expression}"),
            format!("{expression} + ' value'"),
        ] {
            let source = format!(
                "export function answer(event: {{ answered_by: {{ identity_id: string }} }}): any {{ return {expression}; }}"
            );
            lower(&source, Policy::Strict)
                .unwrap_or_else(|error| panic!("strict must admit {source}: {error}"));
        }
    }
    // Explicit conversion facts survive binding, and cell conversion semantics stay available.
    lower(
        "export function answer(event: any): any { const text = JSON.stringify(event); return `${text}`; }",
        Policy::Strict,
    )
    .expect("explicit result binding");
    let cell = crate::lower_with_limits(
        "const value = {}; `${value}`;",
        &environment,
        Mode::Cell(lash_kernel_dialect::CellEnvironment::first()),
        crate::SourceLimits::standard().with_implicit_text_conversion(Policy::Strict),
    );
    assert!(cell.is_ok());
}

/// ENTRY-STRUCTURAL: forward aliases and interfaces keep the same public
/// contract when exported directly or crossed as a callable and promoted.
#[test]
fn structural_entry_annotations_survive_crossing_and_promotion() {
    let declarations = r#"
type Event = Envelope;
interface Envelope { answered_by: Actor; kind: "answer"; [key: string]: JsonValue }
interface Actor { identity_id: string; kind: "human" | "agent" }
"#;
    let source = format!(
        "{declarations}\nexport function handle(event: Event, columns: Record<string, number[]>): Actor {{ return event.answered_by; }}"
    );
    let exported = entries(&source).remove(&Name::new("handle")).unwrap();
    let actor = Type::Record(RecordType {
        fields: vec![
            RecordTypeField {
                name: "identity_id".into(),
                ty: Type::Text,
                optional: false,
            },
            RecordTypeField {
                name: "kind".into(),
                ty: Type::Enum(vec!["human".into(), "agent".into()]),
                optional: false,
            },
        ],
        rest: None,
    });
    assert_eq!(
        exported,
        Signature {
            params: vec![
                param(
                    "event",
                    Type::Record(RecordType {
                        fields: vec![
                            RecordTypeField {
                                name: "answered_by".into(),
                                ty: actor.clone(),
                                optional: false
                            },
                            RecordTypeField {
                                name: "kind".into(),
                                ty: Type::Enum(vec!["answer".into()]),
                                optional: false
                            },
                        ],
                        rest: Some(Box::new(Type::Json)),
                    })
                ),
                param(
                    "columns",
                    Type::Record(RecordType {
                        fields: vec![],
                        rest: Some(Box::new(Type::List(Box::new(Type::Number))))
                    })
                )
            ],
            result: actor
        }
    );
    let lowered = module(&source).unwrap();
    let printed = crate::print(
        &lowered.document,
        Some(&lowered.captions),
        super::library(),
        Default::default(),
    )
    .unwrap();
    let (read, _) = crate::read(
        &printed.source,
        &lowered.document,
        super::library(),
        Default::default(),
        None,
    )
    .unwrap();
    assert_eq!(read.canonical_bytes(), lowered.document.canonical_bytes());
    let spaced = source.replace("return event", "return     event");
    assert_eq!(
        module(&spaced).unwrap().document.identity(),
        lowered.document.identity()
    );
    // A nested alias cannot rebind the outer alias's dependencies.
    let shadowed = entries(
        "type Actor = {id: string}; type Event = {actor: Actor}; export function handle(event: Event): any { type Actor = number; return event.actor; }",
    );
    assert!(
        matches!(&shadowed[&Name::new("handle")].params[0].ty, Type::Record(event) if matches!(&event.fields[0].ty, Type::Record(_)))
    );
    let crossed = super::machine::end(&format!(
        "{declarations}\nconst handle = (event: Event, columns: Record<string, number[]>): Actor => event.answered_by; await control.finish(handle);"
    ));
    let super::machine::Ended::Finished(lash_kernel_doc::Datum::FunctionValue(value)) = crossed
    else {
        panic!("{crossed:?}")
    };
    assert_eq!(value.signature, exported);
    let presentation = lash_kernel_doc::CallablePresentation {
        signature: value.signature.clone(),
        representation: value.representation.clone(),
        convention: value.convention.clone(),
    };
    let promoted = presentation
        .promote_entry(
            value.callable.function,
            lash_vocabulary::FunctionLocation {
                name: Some("handle".into()),
                site: value.creation.clone(),
            },
            false,
            false,
            None,
        )
        .unwrap();
    assert_eq!(promoted.signature, exported);
}

/// ENTRY-SETTLED: resolve aliases before unwrapping every Promise layer.
#[test]
fn promise_alias_entry_results_store_the_settled_structure() {
    let signature = entries(
        r#"
type Pending = Promise<Later>;
type Later = Promise<Answer>;
interface Answer { value: "done" }
export async function handle(): Pending { return {value: "done"}; }
"#,
    )
    .remove(&Name::new("handle"))
    .unwrap();
    assert_eq!(
        signature.result,
        Type::Record(RecordType {
            fields: vec![RecordTypeField {
                name: "value".into(),
                ty: Type::Enum(vec!["done".into()]),
                optional: false
            }],
            rest: None
        })
    );
}

/// ENTRY-WRITTEN: explicit unconstrained annotations mean JSON data;
/// ordinary omitted annotations stay free, but entries require written types.
#[test]
fn entry_annotations_distinguish_written_any_from_missing() {
    let signature = entries("export function handle(event: {x: any; ys: unknown[]}, other: Record<string, any>): Promise<any> { return null; }").remove(&Name::new("handle")).unwrap();
    assert_eq!(signature.result, Type::Json);
    assert_eq!(
        signature.params[1].ty,
        Type::Record(RecordType {
            fields: vec![],
            rest: Some(Box::new(Type::Json))
        })
    );
    assert_eq!(
        signature.params[0].ty,
        Type::Record(RecordType {
            fields: vec![
                RecordTypeField {
                    name: "x".into(),
                    ty: Type::Json,
                    optional: false
                },
                RecordTypeField {
                    name: "ys".into(),
                    ty: Type::List(Box::new(Type::Json)),
                    optional: false
                },
            ],
            rest: None
        })
    );
    super::lower("function ordinary(x) { return x; }").unwrap();
    for (source, parameter) in [
        ("export function handle(event): any { return event; }", true),
        (
            "export function handle(event: any) { return event; }",
            false,
        ),
    ] {
        let diagnostic = module(source).unwrap_err();
        let Some(lash_kernel_dialect::DiagnosticFact::SignatureRequired(required)) =
            diagnostic.fact.as_deref()
        else {
            panic!("typed missing annotation: {diagnostic:?}");
        };
        assert_eq!(required.function.name.as_deref(), Some("handle"));
        let span = required.span.unwrap();
        if parameter {
            assert_eq!(&source[span.start..span.end], "event");
        } else {
            assert_eq!(span.start, source.find('{').unwrap());
            assert_eq!(span.start, span.end);
        }
        let cell = format!(
            "{} await control.finish(handle);",
            source.replace("export ", "       ")
        );
        let super::machine::Ended::Finished(lash_kernel_doc::Datum::FunctionValue(value)) =
            super::machine::end(&cell)
        else {
            panic!("callable crosses before promotion");
        };
        let lowered = super::lower_with_effects(&cell).unwrap();
        let transported: lash_kernel_doc::FunctionValue =
            serde_json::from_value(serde_json::to_value(value).unwrap()).unwrap();
        let lash_kernel_doc::FunctionValueRefusal::SignatureRequired { source: saved } =
            transported
                .entry_export(
                    &lowered.document,
                    &lash_vm_releases::helper_release().unwrap().helpers,
                    &lash_kernel_doc::Selection::Fresh,
                )
                .unwrap_err()
        else {
            panic!("saved missing annotation stays typed");
        };
        assert_eq!(saved.span, required.span);
        assert_eq!(saved.position, required.position);
    }
    let arrow = "export const handle = (event: any) /* => */ => /* => */ event;";
    let error = module(arrow).unwrap_err();
    assert_eq!(error.span.unwrap().start, arrow.find("=> /*").unwrap());
    let error =
        module("type Promise<T> = T; export async function f(): Promise<string> { return 'x'; }")
            .unwrap_err();
    assert!(matches!(
        error.fact.as_deref(),
        Some(lash_kernel_dialect::DiagnosticFact::EntryAnnotationRefused(
            _
        ))
    ));
    use lash_vocabulary::AnnotationResolutionReason as Reason;
    for (declaration, annotation, reason) in [
        (
            "type Cycle = Cycle;",
            "Cycle",
            Reason::Cycle {
                name: "Cycle".into(),
            },
        ),
        (
            "",
            "Missing",
            Reason::Unresolved {
                name: "Missing".into(),
            },
        ),
        (
            "type Generic<T> = T;",
            "Generic",
            Reason::Unsupported {
                construct: "a generic type alias".into(),
            },
        ),
        (
            "",
            "() => number",
            Reason::Unsupported {
                construct: "a callable type".into(),
            },
        ),
        (
            "",
            "bigint",
            Reason::Unsupported {
                construct: "a `bigint` type".into(),
            },
        ),
        (
            "",
            "{[K in string]: number}",
            Reason::Unsupported {
                construct: "a mapped type".into(),
            },
        ),
        (
            "",
            "string extends number ? string : number",
            Reason::Unsupported {
                construct: "a conditional type".into(),
            },
        ),
    ] {
        let source = format!(
            "{declaration} export function handle(event: {annotation}): any {{ return event; }}"
        );
        let diagnostic = module(&source).unwrap_err();
        let Some(lash_kernel_dialect::DiagnosticFact::EntryAnnotationRefused(refused)) =
            diagnostic.fact.as_deref()
        else {
            panic!("typed resolution refusal: {diagnostic:?}");
        };
        let encoded = serde_json::to_value(refused).unwrap();
        assert_eq!(
            serde_json::from_value::<lash_vocabulary::EntryAnnotationRefusal>(encoded).unwrap(),
            *refused
        );
        assert_eq!(refused.reason, reason);
        assert!(refused.span.is_some());
        let cell = format!(
            "{} await control.finish(handle);",
            source.replace("export ", "       ")
        );
        let super::machine::Ended::Finished(lash_kernel_doc::Datum::FunctionValue(value)) =
            super::machine::end(&cell)
        else {
            panic!("ordinary unsupported annotations remain free");
        };
        let lowered = super::lower_with_effects(&cell).unwrap();
        let lash_kernel_doc::FunctionValueRefusal::EntryAnnotation { source: saved } = value
            .entry_export(
                &lowered.document,
                &lash_vm_releases::helper_release().unwrap().helpers,
                &lash_kernel_doc::Selection::Fresh,
            )
            .unwrap_err()
        else {
            panic!("saved resolution reason stays typed");
        };
        assert_eq!(saved.reason, reason);
    }
    let error =
        module("export function handle({x}: {x: string}): string { return x; }").unwrap_err();
    assert!(
        matches!(error.fact.as_deref(), Some(lash_kernel_dialect::DiagnosticFact::EntryAnnotationRefused(refused)) if matches!(refused.position, lash_vocabulary::SignaturePosition::Parameter { index: 0, .. }))
    );
    let patterns = [
        "const f = ({x}: {x: string}): string => x;",
        "const f = ({ x } /* public */ : { x : string }): string => x;",
    ]
    .map(|source| super::lower(source).unwrap().document.identity());
    assert_eq!(patterns[0], patterns[1]);
    let namespaces = Default::default();
    let environment = Environment::new(super::library(), &namespaces);
    for (depth, nodes) in [(1, 4096), (64, 1)] {
        let error = crate::lower_with_limits(
            "export function f(x: {a: string}): any { return x; }",
            &environment,
            Mode::Module,
            crate::SourceLimits::standard()
                .with_entry_annotation_limits(depth, nodes)
                .unwrap(),
        )
        .unwrap_err();
        assert!(
            matches!(error.fact.as_deref(), Some(lash_kernel_dialect::DiagnosticFact::EntryAnnotationRefused(refused)) if refused.reason == Reason::Limit)
        );
    }
}

/// ENTRY-FACTS: the implicit-text policy sees a nested actor through a
/// forward alias, rather than losing its record shape to an unknown fact.
#[test]
fn forward_entry_aliases_supply_record_operand_facts() {
    let diagnostic = module(
        r#"
type Event = Later;
interface Later { answered_by: Actor }
interface Actor { identity_id: string }
export function handle(event: Event): string { return `${event.answered_by}`; }
"#,
    )
    .unwrap_err();
    assert_eq!(diagnostic.code, DiagnosticCode::ImplicitTextConversion);
    assert!(
        matches!(
            diagnostic.operand_type.as_deref(),
            Some(lash_kernel_dialect::SourceOperandType::Record(_))
        ),
        "{diagnostic:?}"
    );
}

/// K-CALLABLE-ROUNDTRIP: frozen bindings stay out of the fragment and readback.
#[test]
fn callable_roundtrip_preserves_the_defining_document_and_capture_bytes() {
    use lash_kernel_dialect::{CallableSourceBase, CallableSourceCodec};
    use lash_kernel_doc::{CaptureValue, Datum, FunctionValue, Node};
    let lowered =
        super::lower("const secret = 7; const answer = (x: number): number => x + secret;")
            .unwrap();
    let (creation, written) = lowered.written.iter().next().unwrap();
    let Node::Expr(lash_kernel_doc::Expr::Closure(closure)) =
        lowered.document.node(creation).unwrap()
    else {
        panic!("closure");
    };
    let presentation = closure.presentation.as_ref().unwrap();
    let base = CallableSourceBase {
        document: lowered.document.clone().into(),
        function: FunctionValue {
            written_signature: Some(k_written(written)),
            signature_origin: written.signature_origin.clone(),
            implicit_text_conversions: written.implicit_text_conversions.clone(),
            callable: lash_vocabulary::CallableId {
                link: lash_vocabulary::LinkId::from_bytes([1; 32]),
                function: closure.function,
            },
            captures: [(
                lash_vocabulary::Name::new("secret"),
                CaptureValue::Data(Datum::Float(lash_kernel_doc::Float::new(7.0))),
            )]
            .into(),
            signature: presentation.signature.clone(),
            convention: presentation.convention.clone(),
            captions: lowered.captions.clone(),
            representation: presentation.representation.clone(),
            creation: Some(creation.clone()),
            token: Some(vec![
                Datum::Text("ts.function".into()),
                Datum::Text(String::new()),
                Datum::Text("answer".into()),
                Datum::Float(lash_kernel_doc::Float::new(1.0)),
                Datum::Null,
                Datum::Record(Vec::new()),
            ]),
        },
    };
    let capture_bytes = lash_vocabulary::tokens::encode_body(&base.function.captures);
    let codec = crate::TypeScript::default();
    let printed =
        CallableSourceCodec::print(&codec, &base, super::library(), Default::default()).unwrap();
    assert!(
        !printed.source.contains("secret ="),
        "capture values never become source"
    );
    let read = CallableSourceCodec::read(
        &codec,
        &printed.source,
        &base,
        super::library(),
        Default::default(),
        None,
    )
    .unwrap();
    assert_eq!(read.document, base.document);
    assert_eq!(read.function, base.function.callable.function);
    assert_eq!(
        lash_vocabulary::tokens::encode_body(&base.function.captures),
        capture_bytes
    );
}

fn k_written(written: &lash_kernel_dialect::Written) -> lash_kernel_doc::WrittenSignature {
    lash_kernel_doc::WrittenSignature {
        language: written.language,
        signature: written.signature.clone(),
    }
}
