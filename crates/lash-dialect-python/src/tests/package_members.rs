//! Package code in Python source (FIG-6922): package paths by their
//! Python spelling, member selection by brand, sealed package values, and
//! code members as function values, over the `dataframe` fixture
//! (FIG-6917) installed beside lash's own packages.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use lash_kernel_dialect::{
    CellEnvironment, Environment, Export, ExportView, Member, Mode, NamedLibrary, Namespaces,
    Offered, PackagePath, QualifiedOperation, Segment,
};
use lash_kernel_doc::{
    ContractId, Datum, EffectName, ErrorDatum, FunctionId, FunctionName, FunctionRegistry, Handle,
    Integer, Name, OperationRequirement, Param, RecordType, RecordTypeField, Signature, Timestamp,
    Type,
};
use lash_kernel_package::{HeldPackages, Package, testing};
use lash_kernel_vm::{
    Bounds, CellStart, EffectRequest, End, Host, KernelMachine, Machine, Outcome, PreparedLibrary,
    Program, Request, Step, Target,
};

use crate::Code;

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

/// The host of these laws: it records the lines a run prints.
#[derive(Default)]
struct Console {
    printed: Vec<Datum>,
}

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
    fn print(&mut self, value: &Datum) {
        self.printed.push(value.clone());
    }
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

fn plan_signature(result: Type) -> Signature {
    Signature {
        params: vec![Param {
            name: Name::new("plan"),
            ty: Type::Any,
            optional: false,
        }],
        result,
    }
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
        let mut effects = super::machine::effects();
        effects.insert(
            EffectName::new("control.finish").expect("an effect name"),
            Signature {
                params: vec![Param {
                    name: Name::new("x"),
                    ty: Type::Any,
                    optional: false,
                }],
                result: Type::Any,
            },
        );
        let mut operations =
            Namespaces::of_effects(&effects, &ContractId::new("1"), super::machine::controls())
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
            operations.insert(
                operation,
                Offered::tool(
                    format!("dataframe.{name}"),
                    OperationRequirement::new(
                        plan_signature(result),
                        ContractId::new(format!("dataframe.{name}/1")),
                    ),
                ),
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

fn lower_in(
    source: &str,
    mode: Mode<'_>,
) -> Result<lash_kernel_dialect::Lowered, lash_kernel_dialect::Diagnostic> {
    let fixture = fixture();
    let environment = Environment::new(&fixture.library, &fixture.namespaces);
    crate::lower(source, &environment, mode)
}

fn lower(source: &str) -> Result<lash_kernel_dialect::Lowered, lash_kernel_dialect::Diagnostic> {
    lower_in(source, Mode::Cell(CellEnvironment::first()))
}

fn refused(source: &str) -> lash_kernel_dialect::Diagnostic {
    lower(source).expect_err("the source is refused")
}

/// The fake `dataframe` runner: `run` answers two rows, or fails with the
/// package's error when the plan's last step is `limit(0)`; `explain`
/// answers each step's name and expression count. Any other tool answers
/// its argument.
fn runner(request: &EffectRequest) -> Outcome {
    let effect = request.effect.as_str();
    if !effect.starts_with("dataframe.") {
        return Outcome::Completed(request.args.first().cloned().unwrap_or(Datum::Null));
    }
    let Some(Datum::List(plan)) = request.args.first() else {
        panic!("a plan: {request:?}");
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

/// What a run ended with: the value `control.finish` or the entry ended
/// with, or the kind and message of the error nothing caught; and the
/// lines it printed.
struct Ran {
    result: Result<Datum, String>,
    printed: Vec<Datum>,
}

/// Runs `lowered` from `target` to its end, answering the tools and the
/// fake runner.
fn run_lowered(lowered: &lash_kernel_dialect::Lowered, target: Target, args: Vec<Datum>) -> Ran {
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
    let mut machine = KernelMachine::start(
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
        Box::new(super::pages::Source::default()),
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let mut console = Console::default();
    let mut finished = None;
    let result = loop {
        match machine.run(&mut console, u64::MAX) {
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
            Ok(Step::Ended(End::Finished(done))) => break Ok(finished.unwrap_or(done.result)),
            Ok(Step::Ended(End::Error(lash_kernel_vm::RunError::Uncaught {
                error: Datum::Error(error),
                ..
            }))) => break Err(format!("{}: {}", error.kind, error.message)),
            other => panic!("{other:?}\n{text}"),
        }
    };
    Ran {
        result,
        printed: console.printed,
    }
}

fn run(source: &str) -> Result<Datum, String> {
    let lowered = lower(source).unwrap_or_else(|error| panic!("{error}"));
    run_lowered(&lowered, Target::Main, Vec::new()).result
}

fn text(value: &str) -> Datum {
    Datum::Text(value.to_owned())
}

fn int(value: i64) -> Datum {
    Datum::Int(Integer::from(value))
}

fn texts(values: &[&str]) -> Datum {
    Datum::List(values.iter().map(|value| text(value)).collect())
}

/// The acceptance program of the host-package design (§2.11), as a
/// workflow entry: no import, short names bound to code members, and one
/// chain whose receivers' types each member's declared result states. The
/// fixture builds its plan by the static `DataQuanta.read_text`, as slice 7
/// declares it, where the design's sketch opens a `Context`.
const WORDCOUNT: &str = r#"async def wordcount(text: str):
    col, lit, count = dataframe.col, dataframe.lit, dataframe.count
    via = (dataframe.DataQuanta.read_text(text).flat_map(lambda l: l.lower().split(), output="utf8")
           .filter(col("value").neq(lit(""))).reduce_by_key([col("value")], [count().alias("n")])
           .sort(col("n").sort(ascending=False), col("value")).limit(5))
    return [f"{w} {n}" for w, n in await via.collect_rows()] + [str(await via.explain())]
"#;

/// The lowered §2.11 program, as kernel text.
const WORDCOUNT_GOLDEN: &str = include_str!("wordcount.kernel");

/// The §2.11 program lowers to its golden byte for byte. It invokes the
/// fixture's members by their identities and performs, through their
/// bodies, the fixture's two operations, as slice 7's hand-written
/// documents and the TypeScript program do; and it runs against the fake
/// runner.
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
        )
        .result,
        Ok(texts(&[
            "a 2",
            "b 1",
            "flat_map:0+fn filter:1 reduce_by_key:2 sort:2 limit:0+5"
        ]))
    );
}

/// PKG-NAMESPACE, Python form: a package member resolves by path in
/// Python's spelling; `import` is refused with the namespace repair; a
/// namespace bound as a value is refused.
#[test]
fn package_members_resolve_by_their_python_spelling_and_namespaces_are_no_values() {
    let import = refused("from dataframe import col\n");
    assert_eq!(import.code, Code::ImportUnsupported.as_str());
    assert!(
        import
            .repairs
            .iter()
            .any(|repair| repair.contains("`col = dataframe.col`")),
        "{import:?}"
    );
    for source in [
        "df = dataframe",
        "T = dataframe.DataQuanta",
        "E = dataframe.ExecutionError",
        "await host.echo(dataframe)",
        "dataframe()",
        "dataframe.DataQuanta(1)",
    ] {
        assert_eq!(
            refused(source).code,
            Code::PackageNamespaceValue.as_str(),
            "{source}"
        );
    }
    for (source, repair) in [
        (
            "dataframe.data_quanta.read_text(\"x\")",
            "`dataframe.DataQuanta`",
        ),
        (
            "dataframe.DataQuanta.readText(\"x\")",
            "`dataframe.DataQuanta.read_text`",
        ),
        ("dataframe.Col(\"x\")", "`dataframe.col`"),
        ("dataframe.DataQuanta.limit(5)", "`value.limit(...)`"),
        (
            "dataframe.DataQuanta.create()",
            "the statics the prompt lists",
        ),
        ("dataframe.head(1)", "the exports the prompt lists"),
        (
            "await dataframe.DataQuanta.read_text(\"x\").collectRows()",
            "`collect_rows`",
        ),
    ] {
        let error = refused(source);
        assert_eq!(error.code, Code::PackageMemberUnknown.as_str(), "{source}");
        assert!(
            error.repairs.iter().any(|offered| offered.contains(repair)),
            "{source}: {error:?}"
        );
    }
    // The escape writes a segment as it is.
    assert_eq!(
        run("await control.finish(getattr(dataframe, \"col\")(\"v\").name)"),
        Ok(text("v"))
    );
}

/// PKG-DISPATCH, Python form: selection and the missing-member raise
/// precede the arguments and survive a wait. On an unknown receiver
/// `data_quanta.sort` and `expr.sort` select their own members, a list's
/// `sort` stays the dialect's, and the property `expr.name` reads as a
/// getter.
#[test]
fn selection_precedes_arguments_and_survives_a_wait() {
    let program = r#"
def by_keys(value, a, b):
    return value.sort(a, b)
def ordered(value):
    return value.sort(ascending=False)
def in_place(value):
    value.sort()
    return value
def name(value):
    return value.name
async def waited(value):
    return value.sort(ascending=await host.echo(True))
expr = ordered(dataframe.col("n"))
quanta = by_keys(dataframe.DataQuanta.read_text("x"), dataframe.col("n"), dataframe.col("v"))
late = await waited(dataframe.col("w"))
try:
    name({"name": "own"})
except AttributeError as error:
    own = str(error)
await control.finish([name(expr), await quanta.explain(), in_place([3, 1, 2]), name(late), own])
"#;
    assert_eq!(
        run(program),
        Ok(Datum::List(vec![
            text("n"),
            text("sort:2"),
            Datum::List(vec![int(1), int(2), int(3)]),
            text("w"),
            text("'dict' object has no attribute 'name'"),
        ]))
    );
    // A package value without the member raises before any argument runs.
    let raised = r#"
ran = []
def touch():
    ran.append(1)
    return 0
def pick(value):
    return value.limit(touch())
try:
    pick(dataframe.col("x"))
except AttributeError as error:
    await control.finish([str(error), len(ran)])
"#;
    assert_eq!(
        run(raised),
        Ok(Datum::List(vec![
            text("'Expr' object has no attribute 'limit'"),
            int(0),
        ]))
    );
}

/// A dict's `count` stays `py.method.count`: no package member answers
/// a method of that name, though a package function is called `count`.
#[test]
fn a_dicts_count_stays_the_dialects_method() {
    let source = "d = {\"a\": 1}\nn = d.count(1)\ndef f(value):\n    return value.count(1)\nm = f([1, 2, 1])";
    let lowered = lower(source).unwrap_or_else(|error| panic!("{error}"));
    let named: Vec<&str> = lowered
        .document
        .document()
        .manifest
        .functions
        .values()
        .map(FunctionName::as_str)
        .collect();
    assert!(named.contains(&"py.method.count"), "{named:?}");
    assert!(!named.contains(&"dataframe.count"), "{named:?}");
    assert_eq!(
        run("def f(value):\n    return value.count(1)\nawait control.finish(f([1, 2, 1]))"),
        Ok(int(2))
    );
}

/// A keyword names one field of one options slot: one that names a field
/// of two is refused, naming both slots.
#[test]
fn an_ambiguous_keyword_is_refused_naming_the_slots() {
    struct Held(BTreeMap<PackagePath, BTreeMap<Segment, Export>>);
    impl ExportView for Held {
        fn exports(&self, path: &PackagePath) -> Option<&BTreeMap<Segment, Export>> {
            self.0.get(path)
        }
        fn paths(&self) -> Vec<PackagePath> {
            self.0.keys().cloned().collect()
        }
    }
    let options = |fields: &[&str]| {
        Type::Record(RecordType {
            fields: fields
                .iter()
                .map(|name| RecordTypeField {
                    name: (*name).to_owned(),
                    ty: Type::Int,
                    optional: true,
                })
                .collect(),
            rest: None,
        })
    };
    let slot = |name: &str, ty: Type| Param {
        name: Name::new(name),
        ty,
        optional: false,
    };
    let pick = Member {
        definition: FunctionName::new("opts.pick").expect("a qualified name"),
        signature: Signature {
            params: vec![
                slot("first", options(&["key"])),
                slot(
                    "second",
                    Type::Union(vec![options(&["key", "other"]), Type::Absent]),
                ),
            ],
            result: Type::Int,
        },
        asynchronous: false,
        rest: false,
    };
    let view = Held(BTreeMap::from([(
        PackagePath::parse("opts").expect("canonical"),
        BTreeMap::from([(
            Segment::new("pick").expect("canonical"),
            Export::Function(pick),
        )]),
    )]));
    let namespaces = Namespaces::build(&view, Default::default()).expect("composes");
    let environment = Environment::new(super::machine::library(), &namespaces);
    let error = crate::lower(
        "opts.pick(key=1)",
        &environment,
        Mode::Cell(CellEnvironment::first()),
    )
    .expect_err("the keyword is ambiguous");
    assert_eq!(error.code, Code::Arguments.as_str());
    assert!(error.message.contains("`first` and `second`"), "{error:?}");
}

/// SEALED, Python form: a field of a package value is refused at lowering
/// where its type is known, naming the type. Where it is not, subscript
/// read, write and delete, `len`, iteration and `dict()` raise
/// `TypeError`, `keys()` raises `AttributeError`, and the value prints as
/// its type, never as its fields.
#[test]
fn a_field_of_a_package_value_is_sealed() {
    for source in [
        "dataframe.col(\"x\").op",
        "dataframe.col(\"x\").plan()",
        "dataframe.DataQuanta.read_text(\"x\").plan",
        "dataframe.DataQuanta.read_text(\"x\").keys()",
    ] {
        let error = refused(source);
        assert_eq!(error.code, Code::PackageMemberUnknown.as_str(), "{source}");
        assert!(error.message.contains("have no member"), "{error}");
    }
    let program = r#"
q = dataframe.DataQuanta.read_text("x")
caught = []
def attempt(f):
    try:
        f()
    except TypeError as error:
        caught.append("TypeError: " + str(error))
    except AttributeError as error:
        caught.append("AttributeError: " + str(error))
def store():
    q["x"] = 1
def delete():
    del q["x"]
def walk():
    for k in q:
        pass
attempt(lambda: q["plan"])
attempt(store)
attempt(delete)
attempt(lambda: len(q))
attempt(walk)
attempt(lambda: dict(q))
attempt(lambda: q.keys())
print(q)
await control.finish(caught + [str(q), repr(q), f"{q}"])
"#;
    let lowered = lower(program).unwrap_or_else(|error| panic!("{error}"));
    let ran = run_lowered(&lowered, Target::Main, Vec::new());
    assert_eq!(
        ran.result,
        Ok(texts(&[
            "TypeError: 'DataQuanta' object is not subscriptable",
            "TypeError: 'DataQuanta' object does not support item assignment",
            "TypeError: 'DataQuanta' object doesn't support item deletion",
            "TypeError: object of type 'DataQuanta' has no len()",
            "TypeError: 'DataQuanta' object is not iterable",
            "TypeError: 'DataQuanta' object is not iterable",
            "AttributeError: 'DataQuanta' object has no attribute 'keys'",
            "<dataframe.DataQuanta>",
            "<dataframe.DataQuanta>",
            "<dataframe.DataQuanta>",
        ]))
    );
    assert_eq!(ran.printed, vec![text("<dataframe.DataQuanta>")]);
}

/// MEMBER-VALUE, Python form: a code member as a value calls that
/// identity; an operation member as a value is refused.
#[test]
fn a_code_member_is_a_function_value_and_an_operation_is_not() {
    let program = r#"
col, literal = dataframe.col, dataframe.lit
make = dataframe.col
def apply(f, x):
    return f(x)
try:
    col()
except TypeError as error:
    missing = str(error)
await control.finish([col("a").name, make("b").name, apply(dataframe.col, "c").name, literal(1).name, missing])
"#;
    assert_eq!(
        run(program),
        Ok(texts(&[
            "a",
            "b",
            "c",
            "lit",
            "col() missing required argument: 'name'"
        ]))
    );
    for source in ["run = dataframe.run", "f = host.echo"] {
        assert_eq!(
            refused(source).code,
            Code::OperationValue.as_str(),
            "{source}"
        );
    }
}

/// `except` names a package error by its path, and catches the error's
/// kernel kind.
#[test]
fn except_catches_a_package_error_by_its_kind() {
    let program = r#"
try:
    await dataframe.DataQuanta.read_text("x").limit(0).collect_rows()
except dataframe.ExecutionError as error:
    await control.finish(str(error))
"#;
    assert_eq!(run(program), Ok(text("nothing to collect")));
}

/// K-DIALECT-001: the lowered §2.11 program prints, and the printed source
/// reads back to the same document bytes.
#[test]
fn a_package_program_prints_and_reads_back_byte_exact() {
    let lowered = lower_in(WORDCOUNT, Mode::Module).expect("lowers");
    let document = lowered.document.document();
    let source = crate::print(document, None, Default::default())
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
