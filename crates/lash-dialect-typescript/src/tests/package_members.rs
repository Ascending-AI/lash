//! Package code in TypeScript source (FIG-6920): package paths by their
//! TypeScript spelling, member selection by brand, sealed package values,
//! and code members as function values, over the `dataframe` fixture
//! (FIG-6917) installed beside lash's own packages.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use lash_kernel_dialect::{
    CellEnvironment, Environment, Mode, NamedLibrary, Namespaces, Offered, Printer,
    QualifiedOperation,
};
use lash_kernel_doc::{
    Datum, EffectName, ErrorDatum, FunctionId, FunctionRegistry, Handle, Integer, Name, Param,
    Signature, Timestamp, Type,
};
use lash_kernel_package::{HeldPackages, Package, testing};
use lash_kernel_vm::{
    Bounds, CellStart, EffectRequest, End, Host, Machine, Outcome, PreparedLibrary, Program,
    Request, Step, Target,
};

use crate::{Diagnostic, DiagnosticCode, TypeScript};

const BOUNDS: Bounds = Bounds {
    memory_meter: lash_kernel_vm::MemoryMeter::On,
    charge: lash_kernel_vm::Charge::Exact,
    charge_limit: 10_000_000,
    memory: 64 << 20,
    call_depth: 200,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

#[derive(Default)]
struct Console;

impl Host for Console {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(0),
        }
    }
    fn random(&mut self) -> u64 {
        0
    }
    fn read(&mut self, _handle: &Handle, _request: &Datum) -> Result<Datum, ErrorDatum> {
        panic!("these laws read no projection")
    }
    fn print(&mut self, _value: &Datum) {}
    fn cancel_requested(&mut self) -> bool {
        false
    }
}

/// The fixture installed beside lash's packages: the worker registry and
/// library it lowers and runs against, and the namespaces its set and
/// the laws' operations compose.
struct Fixture {
    dataframe: Package,
    library: NamedLibrary,
    prepared: PreparedLibrary,
    namespaces: Namespaces,
}

fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let lash = lash_vm_releases::built().expect("lash's packages");
        let kernel = lash
            .iter()
            .find(|package| package.path().to_string() == "kernel")
            .expect("the kernel package");
        let dataframe = testing::dataframe(kernel).expect("the fixture builds");
        let held = HeldPackages::install(lash.iter().cloned().chain([dataframe.clone()]))
            .expect("one registration");
        let embedding = lash_vm_worker::built_dialects(&Default::default())
            .and_then(|embedder| embedder.finish(&held))
            .expect("the worker binds to the registration");
        let registry: Arc<FunctionRegistry> =
            Arc::clone(embedding.registry().expect("the worker registry"));
        let view = held.view(&held.own()).expect("the node's own set");
        let mut operations = Namespaces::of_signatures(&effects(), super::controls())
            .expect("the laws' tools are operations")
            .operations()
            .clone();
        for (name, result) in [
            ("run", Type::List(Box::new(Type::Any))),
            ("explain", Type::Text),
        ] {
            let operation = QualifiedOperation::from_effect(
                &EffectName::new(format!("dataframe.{name}")).expect("an effect name"),
            )
            .expect("an operation");
            let signature = Signature {
                params: vec![Param {
                    name: Name::new("plan"),
                    ty: Type::Any,
                    optional: false,
                }],
                result,
            };
            operations.insert(
                operation,
                Offered::tool(format!("dataframe.{name}"), signature),
            );
        }
        Fixture {
            library: NamedLibrary::from_registry(&registry).expect("unique names"),
            prepared: PreparedLibrary::new(registry),
            namespaces: Namespaces::build(&view, operations).expect("the namespaces compose"),
            dataframe,
        }
    })
}

/// The laws' tools: `host.echo(x)` answers `x`, `control.finish(x)` ends.
fn effects() -> BTreeMap<EffectName, Signature> {
    super::machine::effects()
}

fn lower_in(source: &str, mode: Mode<'_>) -> Result<lash_kernel_dialect::Lowered, Diagnostic> {
    let fixture = fixture();
    let environment = Environment::new(&fixture.library, &fixture.namespaces);
    crate::lower(source, &environment, mode)
}

fn lower(source: &str) -> Result<lash_kernel_dialect::Lowered, Diagnostic> {
    lower_in(source, Mode::Cell(CellEnvironment::first()))
}

fn refused(source: &str) -> Diagnostic {
    lower(source).expect_err("the source is refused")
}

/// The fake `dataframe` runner: `run` answers two rows, or fails with the
/// package's error when the plan's last step is `limit(0)`; `explain`
/// answers each step's name and expression count.
fn runner(request: &EffectRequest) -> Outcome {
    let effect = request.effect.as_str();
    let plan = match request.args.first() {
        Some(Datum::List(plan)) if effect.starts_with("dataframe.") => plan,
        argument if !effect.starts_with("dataframe.") => {
            return Outcome::Completed(argument.cloned().unwrap_or(Datum::Null));
        }
        _ => panic!("a plan: {request:?}"),
    };
    let field = |step: &Datum, name: &str| match step {
        Datum::Record(fields) => fields
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.clone()),
        other => panic!("a plan step: {other:?}"),
    };
    match effect {
        "dataframe.run" => match plan.last().and_then(|last| field(last, "n")) {
            Some(Datum::Int(n)) if n == Integer::from(0) => Outcome::Failed(ErrorDatum {
                kind: "dataframe.execution_error".to_owned(),
                message: "nothing to collect".to_owned(),
                data: Datum::Null,
            }),
            _ => Outcome::Completed(Datum::List(vec![
                Datum::List(vec![text("a"), Datum::Int(Integer::from(2))]),
                Datum::List(vec![text("b"), Datum::Int(Integer::from(1))]),
            ])),
        },
        "dataframe.explain" => Outcome::Completed(text(
            &plan
                .iter()
                .map(|step| {
                    let Some(Datum::Text(name)) = field(step, "step") else {
                        panic!("a named step: {step:?}");
                    };
                    let Some(Datum::List(exprs)) = field(step, "exprs") else {
                        panic!("a step's expressions: {step:?}");
                    };
                    let callback = match field(step, "callback") {
                        Some(Datum::Function(_) | Datum::FunctionValue(_)) => "+fn",
                        Some(other) => panic!("a callback crosses as a function: {other:?}"),
                        None => "",
                    };
                    let n = match field(step, "n") {
                        Some(Datum::Int(n)) => format!("+{n}"),
                        Some(other) => panic!("`limit` takes an integer: {other:?}"),
                        None => String::new(),
                    };
                    format!("{name}:{}{callback}{n}", exprs.len())
                })
                .collect::<Vec<_>>()
                .join(" "),
        )),
        _ => panic!("the fixture offers no other operation: {request:?}"),
    }
}

/// Runs `lowered` from `target` to its end, answering the tools and the
/// fake runner: the value `control.finish` or the entry ended with, or the
/// kind and message of the error nothing caught.
fn run_lowered(
    lowered: &lash_kernel_dialect::Lowered,
    target: Target,
    args: Vec<Datum>,
) -> Result<Datum, String> {
    let text = lash_kernel_doc::print_document(&lowered.document);
    if let Err(invalid) = lash_kernel_doc::validate_document(&lowered.document, &fixture().library)
    {
        panic!("{invalid}\n{text}");
    }
    let program = Program {
        closure: super::pages::CLOSURE,
        document: Arc::clone(lowered.document.shared()),
        library: fixture().prepared.clone(),
    };
    let mut machine = super::pages::start(
        program,
        BOUNDS,
        CellStart {
            target,
            args,
            session: None,
            owner: super::pages::OWNER,
            modes: Default::default(),
            host: Vec::new(),
        },
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let mut finished = None;
    loop {
        match machine.run(&mut Console, u64::MAX) {
            Ok(Step::Slice) => {}
            Ok(Step::Parked(park)) => {
                for request in &park.requests {
                    let Request::Effect(effect) = request else {
                        panic!("only effects are answered: {park:?}\n{text}");
                    };
                    if effect.effect.as_str() == "control.finish" {
                        finished = effect.args.first().cloned();
                    }
                    machine
                        .deliver(effect.wait, runner(effect))
                        .unwrap_or_else(|error| panic!("{error}"));
                }
            }
            Ok(Step::Ended(End::Finished(done))) => {
                return Ok(finished.unwrap_or(done.result));
            }
            Ok(Step::Ended(End::Error(lash_kernel_vm::RunError::Uncaught {
                error: Datum::Error(error),
                ..
            }))) => return Err(format!("{}: {}", error.kind, error.message)),
            Ok(Step::Ended(End::Error(lash_kernel_vm::RunError::Uncaught { error, .. }))) => {
                return Err(format!("{error:?}"));
            }
            other => panic!("{other:?}\n{text}"),
        }
    }
}

fn run(source: &str) -> Result<Datum, String> {
    let lowered = lower(source).unwrap_or_else(|error| panic!("{error}"));
    run_lowered(&lowered, Target::Main, Vec::new())
}

fn text(value: &str) -> Datum {
    Datum::Text(value.to_owned())
}

fn float(value: f64) -> Datum {
    Datum::Float(lash_kernel_doc::Float::new(value))
}

/// The acceptance program of the host-package design (§2.11), as a
/// workflow entry: no import, short names bound to code members, and one
/// chain whose receivers' types each member's declared result states. The
/// fixture builds its plan by the static `DataQuanta.readText`, as slice 7
/// declares it, where the design's sketch opens a `Context`.
const WORDCOUNT: &str = r#"export async function wordcount(text: string) {
  const { col, lit, count } = dataframe;
  const viaExprs = dataframe.DataQuanta.readText(text)
    .flatMap((line: string) => line.toLowerCase().split(/\W+/), { output: "utf8" })
    .filter(col("value").neq(lit("")))
    .reduceByKey([col("value")], [count().alias("n")])
    .sort(col("n").sort({ ascending: false }), col("value"))
    .limit(5);
  const lines: string[] = [];
  for (const [word, n] of (await viaExprs.collectRows()) as [string, number][]) lines.push(`${word} ${n}`);
  lines.push(String(await viaExprs.explain()));
  return lines;
}
"#;

/// The lowered §2.11 program, as kernel text.
const WORDCOUNT_GOLDEN: &str = include_str!("wordcount.kernel");

/// The §2.11 program lowers to its golden byte for byte. It invokes the
/// fixture's members by their identities and performs, through their
/// bodies, the fixture's two operations, as slice 7's hand-written
/// documents do; and it runs against the fake runner.
#[test]
fn the_acceptance_program_lowers_to_its_golden_and_runs() {
    let lowered = lower_in(WORDCOUNT, Mode::Module).unwrap_or_else(|error| panic!("{error}"));
    let printed = lash_kernel_doc::print_document(&lowered.document);
    assert!(
        printed == WORDCOUNT_GOLDEN,
        "the lowered golden drifted; it is now:\n<<<golden\n{printed}>>>golden"
    );
    let dataframe = &fixture().dataframe;
    let invoked: Vec<FunctionId> = [
        "dataframe.col",
        "dataframe.lit",
        "dataframe.count",
        "dataframe.expr.neq",
        "dataframe.expr.alias",
        "dataframe.expr.sort",
        "dataframe.data_quanta.read_text",
        "dataframe.data_quanta.flat_map",
        "dataframe.data_quanta.filter",
        "dataframe.data_quanta.reduce_by_key",
        "dataframe.data_quanta.sort",
        "dataframe.data_quanta.limit",
        "dataframe.data_quanta.collect_rows",
        "dataframe.data_quanta.explain",
    ]
    .map(|name| dataframe.function(name).expect("a fixture member"))
    .to_vec();
    let manifest = &lowered.document.document().manifest;
    for function in &invoked {
        assert!(
            manifest.functions.contains_key(function),
            "{function} is invoked by identity\n{printed}"
        );
    }
    assert_eq!(
        manifest
            .effects
            .keys()
            .map(EffectName::as_str)
            .collect::<Vec<_>>(),
        ["dataframe.explain", "dataframe.run"],
        "the members' bodies perform the fixture's operations"
    );
    assert_eq!(
        run_lowered(
            &lowered,
            Target::Entry(Name::new("wordcount")),
            vec![text("a b a")]
        ),
        Ok(Datum::List(vec![
            text("a 2"),
            text("b 1"),
            text("flat_map:0+fn filter:1 reduce_by_key:2 sort:2 limit:0+5"),
        ]))
    );
}

/// PKG-NAMESPACE: a package member resolves by path in the dialect's
/// spelling; `import` is refused with the namespace repair; a namespace
/// bound as a value is refused.
#[test]
fn package_members_resolve_by_their_typescript_spelling_and_namespaces_are_no_values() {
    let import = refused("import { col } from \"dataframe\";");
    assert_eq!(import.code, DiagnosticCode::ImportExportUnsupported);
    assert!(
        import
            .suggestions
            .iter()
            .any(|repair| repair.contains("const { col } = dataframe")),
        "{import:?}"
    );
    for source in [
        "const df = dataframe;",
        "const T = dataframe.DataQuanta;",
        "const E = dataframe.ExecutionError;",
        "await host.echo(dataframe);",
        "const { ...all } = dataframe;",
        "dataframe();",
    ] {
        assert_eq!(
            refused(source).code,
            DiagnosticCode::PackageNamespaceValue,
            "{source}"
        );
    }
    for (source, repair) in [
        (
            "dataframe.data_quanta.read_text(\"x\");",
            "`dataframe.DataQuanta`",
        ),
        (
            "dataframe.DataQuanta.read_text(\"x\");",
            "`dataframe.DataQuanta.readText`",
        ),
        ("dataframe.Col(\"x\");", "`dataframe.col`"),
        ("dataframe.DataQuanta.limit(5);", "`value.limit(...)`"),
        (
            "dataframe.DataQuanta.create();",
            "the statics the prompt lists",
        ),
        ("dataframe.head(1);", "the exports the prompt lists"),
        (
            "const q = dataframe.DataQuanta.readText(\"x\"); await control.finish(q.collect_rows());",
            "`collectRows`",
        ),
    ] {
        let error = refused(source);
        assert_eq!(error.code, DiagnosticCode::PackageMemberUnknown, "{source}");
        assert!(
            error
                .suggestions
                .iter()
                .any(|offered| offered.contains(repair)),
            "{source}: {error:?}"
        );
    }
    // The escape writes a segment as it is.
    assert_eq!(
        run("await control.finish(dataframe[\"col\"](\"v\").name);"),
        Ok(text("v"))
    );
}

/// PKG-DISPATCH: selection, ordinary-member read and missing-member raise
/// all precede arguments and survive a wait. On an unknown receiver
/// `data_quanta.sort` and `expr.sort` select their own members, and the
/// property `expr.name` reads as a getter.
#[test]
fn selection_precedes_arguments_and_survives_a_wait() {
    let program = r#"
const pick = (value: any, ...keys: any[]) => value.sort(...keys);
const name = (value: any) => value.name;
const expr = pick(dataframe.col("n"), { ascending: false });
const quanta = pick(dataframe.DataQuanta.readText("x"), dataframe.col("n"), dataframe.col("v"));
const plain = pick([3, 1, 2]);
const waited = async (value: any) => value.sort(await host.echo({ ascending: true }));
const late = await waited(dataframe.col("w"));
await control.finish([name(expr), await quanta.explain(), plain, name(late), name({ name: "own" })]);
"#;
    assert_eq!(
        run(program),
        Ok(Datum::List(vec![
            text("n"),
            text("sort:2"),
            Datum::List(vec![float(1.0), float(2.0), float(3.0)]),
            text("w"),
            text("own"),
        ]))
    );
    // A package value without the member raises before any argument runs.
    let raised = r#"
let ran = 0;
const touch = () => { ran = ran + 1; return 0; };
const pick = (value: any) => value.limit(touch());
try { pick(dataframe.col("x")); } catch (error) { await control.finish([String(error), ran]); }
"#;
    assert_eq!(
        run(raised),
        Ok(Datum::List(vec![
            text("TypeError: this package value has no member `limit`"),
            float(0.0),
        ]))
    );
    // An ordinary receiver's method is read before its arguments run.
    let ordinary = r#"
const box = { sort: (x: any) => "first" };
const swap = () => { box.sort = (x: any) => "second"; return 0; };
const pick = (value: any) => value.sort(swap());
await control.finish(pick(box));
"#;
    assert_eq!(run(ordinary), Ok(text("first")));
}

/// SEALED: a field read on a package value is refused at lowering when its
/// type is known, naming the type, and raises the attribute error at run
/// time when it is not.
#[test]
fn a_field_of_a_package_value_is_sealed() {
    for source in [
        "const e = dataframe.col(\"x\"); await control.finish(e.op);",
        "const e = dataframe.col(\"x\"); await control.finish(e.plan());",
        "const q = dataframe.DataQuanta.readText(\"x\"); await control.finish(q.plan);",
        "function f(q: dataframe.DataQuanta) { return q.plan; }",
    ] {
        let error = refused(source);
        assert_eq!(error.code, DiagnosticCode::PackageMemberUnknown, "{source}");
        assert!(error.message.contains("have no member"), "{error}");
    }
    let annotated = r#"
function name(e: dataframe.Expr) { return e.name; }
await control.finish(name(dataframe.col("typed")));
"#;
    assert_eq!(run(annotated), Ok(text("typed")));
    let unknown = r#"
const hide = (value: any) => value;
try { await control.finish(hide(dataframe.col("x")).op); } catch (error) { await control.finish(String(error)); }
"#;
    assert_eq!(
        run(unknown),
        Ok(text("TypeError: this package value has no member `op`"))
    );
}

/// MEMBER-VALUE: a code member as a value calls that identity; an
/// operation member as a value is refused.
#[test]
fn a_code_member_is_a_function_value_and_an_operation_is_not() {
    let program = r#"
const { col, lit: literal } = dataframe;
const make = dataframe.col;
const apply = (f: any, x: any) => f(x);
await control.finish([col("a").name, make("b").name, apply(dataframe.col, "c").name, literal(1).name]);
"#;
    assert_eq!(
        run(program),
        Ok(Datum::List(vec![
            text("a"),
            text("b"),
            text("c"),
            text("lit")
        ]))
    );
    for source in [
        "const run = dataframe.run;",
        "const f = host.echo;",
        "const { run } = dataframe;",
    ] {
        assert_eq!(
            refused(source).code,
            DiagnosticCode::OperationValue,
            "{source}"
        );
    }
}

/// A package error is matched by `instanceof` on its kernel kind, the
/// error's canonical path.
#[test]
fn instanceof_a_package_error_tests_its_kind() {
    let program = r#"
const caught = async (f: any) => { try { await f(); } catch (e) { return e; } };
const failed = await caught(() => dataframe.DataQuanta.readText("x").limit(0).collectRows());
const other = await caught(() => { throw new Error("x"); });
await control.finish([failed instanceof dataframe.ExecutionError, other instanceof dataframe.ExecutionError, 1 instanceof dataframe.ExecutionError]);
"#;
    assert_eq!(
        run(program),
        Ok(Datum::List(vec![
            Datum::Bool(true),
            Datum::Bool(false),
            Datum::Bool(false),
        ]))
    );
}

/// K-DIALECT-001: the lowered §2.11 program prints, and the printed source
/// reads back to the same document bytes.
#[test]
fn a_package_program_prints_and_reads_back_byte_exact() {
    let lowered = lower_in(WORDCOUNT, Mode::Module).expect("lowers");
    let document = lowered.document.document();
    let source = TypeScript::default()
        .print(document, None, Default::default())
        .unwrap_or_else(|error| panic!("{error}"))
        .source;
    let (read, _) = crate::read(&source, document, Default::default())
        .unwrap_or_else(|error| panic!("{error}\n{source}"));
    assert_eq!(
        read.canonical_bytes(),
        document.canonical_bytes(),
        "{source}"
    );
}
