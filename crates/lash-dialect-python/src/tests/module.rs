//! MODULE-CONTRACT (FIG-6380): a Python workflow module under the workflow
//! contract every dialect shares. These cases pin what each Python
//! annotation means; the twin cases that compare entry names and refusal
//! codes with TypeScript run through the host's import.

use std::collections::BTreeMap;

use lash_kernel_dialect::workflow::ModuleRefusal;
use lash_kernel_dialect::{Environment, Lowered, Mode};
use lash_vocabulary::{MapType, Name, Param, Signature, Type};

use super::machine;

fn module(source: &str) -> Result<Lowered, lash_kernel_dialect::Diagnostic> {
    let effects = machine::effects();
    let environment_namespaces = crate::tests::linked::namespaces(&effects, machine::controls())
        .expect("the test's effects are operations");
    let environment = Environment::new(machine::library(), &environment_namespaces);
    let lowered = crate::lower(source, &environment, Mode::Module)?;
    lash_kernel_doc::validate_document(&lowered.document, machine::library())
        .unwrap_or_else(|invalid| panic!("{invalid}"));
    lowered
        .source_map
        .as_ref()
        .expect("a module has its source map")
        .validate(&lowered.document, source)
        .unwrap_or_else(|refused| panic!("{refused}"));
    Ok(lowered)
}

/// FIG-7135: the fan-out printer inverts captured task thunks back to
/// coroutine calls inside gather, retaining the entire kernel identity.
#[test]
fn gathered_local_coroutines_print_without_task_thunks() {
    let source = "import asyncio\n#: Echo twice\nasync def demo() -> list[str]:\n    #: Echo once\n    async def work(value: str):\n        return await host.echo(value)\n    #: Together\n    return await asyncio.gather(work('a'), work('b'))\n";
    let lowered = module(source).expect("fan-out module");
    let printed = crate::print(
        &lowered.document,
        Some(&lowered.captions),
        machine::library(),
        lash_kernel_dialect::PrintOptions {
            source_map: true,
            ..Default::default()
        },
    )
    .unwrap_or_else(|error| panic!("{error}: {:?}", lowered.document));
    let (read, captions) = crate::read(
        &printed.source,
        &lowered.document,
        machine::library(),
        Default::default(),
    )
    .expect("authored fan-out readback");
    assert_eq!(
        lash_kernel_doc::print_document(&read),
        lash_kernel_doc::print_document(&lowered.document)
    );
    assert_eq!(captions, lowered.captions);
    printed
        .source_map
        .expect("printed map")
        .validate(&read, &printed.source)
        .expect("map matches exact readback");
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

fn refusal(source: &str) -> String {
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

fn map(key: Type, value: Type) -> Type {
    Type::Map(Box::new(MapType { key, value }))
}

/// Each annotation's kernel meaning in Python, mapped structurally rather
/// than read as a scalar or any value: containers carry their item types,
/// a dict is a map, and `|` and `Optional` are unions.
#[test]
fn an_entry_signature_is_the_kernel_meaning_of_its_python_annotations() {
    let source = "\
import asyncio


async def digest(
    inbox: str,
    count: int,
    ratio: float,
    flags: list[bool],
    topics: list[dict],
    scores: dict[str, int],
    tags: set[str],
    pair: tuple[str, int],
    note: str | None,
    limit: Optional[int],
    anything: Any,
    untyped,
) -> dict:
    await asyncio.sleep(0)
    return {\"inbox\": inbox}
";
    assert_eq!(
        entries(source),
        BTreeMap::from([(
            Name::new("digest"),
            Signature {
                params: vec![
                    param("inbox", Type::Text),
                    param("count", Type::Int),
                    param("ratio", Type::Float),
                    param("flags", Type::List(Box::new(Type::Bool))),
                    param("topics", Type::List(Box::new(map(Type::Any, Type::Any)))),
                    param("scores", map(Type::Text, Type::Int)),
                    param("tags", Type::Set(Box::new(Type::Text))),
                    param("pair", Type::Tuple(vec![Type::Text, Type::Int])),
                    param("note", Type::Union(vec![Type::Text, Type::Null])),
                    param("limit", Type::Union(vec![Type::Int, Type::Null])),
                    param("anything", Type::Any),
                    param("untyped", Type::Any),
                ],
                result: map(Type::Any, Type::Any),
            },
        )])
    );
}

/// Top-level sync and async defs are Exports; block and nested definitions
/// remain ordinary code. Each Export preserves its source async flag.
#[test]
fn top_level_function_exports_keep_async_flags_and_exclude_block_locals() {
    let source = "\
async def first():
    async def nested():
        return 0
    return await nested()


async def second(n: int) -> int:
    return n * 2


def helper():
    return 7


if helper():
    async def in_block():
        return 5
";
    let lowered = module(source).unwrap();
    let exported = lowered
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
        exported,
        BTreeMap::from([
            (Name::new("first"), true),
            (Name::new("helper"), false),
            (Name::new("second"), true),
        ])
    );
}

/// A workflow names its tools with their namespaces, as the host's catalog
/// does (`display.show_message`): a module calls one by that dotted path,
/// keywords naming its parameters, as a cell calls a tool by its one name.
#[test]
fn a_module_calls_a_namespaced_tool_by_its_dotted_path() {
    let show = lash_vocabulary::EffectName::new("display.show_message").expect("a tool's name");
    let effects = BTreeMap::from([(
        show.clone(),
        Signature {
            params: vec![param("text", Type::Text)],
            result: Type::Any,
        },
    )]);
    let environment_namespaces = crate::tests::linked::namespaces(&effects, &Default::default())
        .expect("the test's effects are operations");
    let environment = Environment::new(machine::library(), &environment_namespaces);
    let lowered = crate::lower(
        "async def demo():\n    return await display.show_message(text=\"hi\")\n",
        &environment,
        Mode::Module,
    )
    .unwrap_or_else(|diagnostic| panic!("{diagnostic}"));
    assert!(
        lowered
            .document
            .imports
            .values()
            .any(|import| import.name.package.to_string() == "display"
                && import.name.member.as_str() == "show_message")
    );
}

/// FIG-6515: host tools take one input record. Python keywords name its
/// declared fields, and the performed argument stays that one record.
#[test]
fn tool_keywords_name_the_declared_input_fields() {
    use lash_kernel_doc::Datum;
    use lash_kernel_vm::{End, Outcome, Request};
    use lash_vocabulary::EffectName;

    let show = EffectName::new("display.show_message").expect("a tool's name");
    let projected = |schema: serde_json::Value| {
        lash_vocabulary::schema::type_of_schema(&schema)
            .expect("a tool's input type")
            .0
    };
    let signature = lash_vocabulary::schema::tool_signature(
        projected(serde_json::json!({
            "type": "object",
            "properties": {
                "text": {"type": "string"},
                "style": {"type": "string"}
            },
            "required": ["text"],
            "additionalProperties": false
        })),
        false,
        lash_vocabulary::Type::Null,
    );
    let effects = BTreeMap::from([(show.clone(), signature.clone())]);
    let environment_namespaces = crate::tests::linked::namespaces(&effects, machine::controls())
        .expect("the test's effects are operations");
    let environment = Environment::new(machine::library(), &environment_namespaces);
    for (call, expected) in [
        (
            "display.show_message(text='hi')",
            vec![("text".to_owned(), Datum::Text("hi".to_owned()))],
        ),
        (
            "display.show_message(style='bold', text='hi')",
            vec![
                ("style".to_owned(), Datum::Text("bold".to_owned())),
                ("text".to_owned(), Datum::Text("hi".to_owned())),
            ],
        ),
    ] {
        let source = format!("async def demo():\n    return await {call}\n");
        let lowered = crate::lower(&source, &environment, Mode::Module)
            .unwrap_or_else(|diagnostic| panic!("{diagnostic}\n{source}"));
        assert_eq!(
            lowered
                .document
                .imports
                .values()
                .find_map(|import| match &import.kind {
                    lash_vocabulary::ImportKind::Operation { signature }
                        if import.name.member.as_str() == "show_message" =>
                        Some(signature.clone()),
                    _ => None,
                })
                .unwrap(),
            signature
        );
        let admission = lash_kernel_check::Environment::new(machine::library());
        lash_kernel_check::admit(&lowered.document, &admission).expect("valid tool input");
        let mut calls = 0;
        // Cells and modules both call it by its dotted path.
        let cell_effect = show.clone();
        let (_, end) = machine::drive(
            &format!("await {call}\n"),
            BTreeMap::from([(cell_effect.clone(), signature.clone())]),
            super::pages::Session::default(),
            |_, park, _| {
                park.requests
                    .into_iter()
                    .map(|request| {
                        let Request::Effect(effect) = request;
                        calls += 1;
                        assert_eq!(crate::tests::linked::operation(&effect.use_), cell_effect);
                        assert_eq!(effect.args, [Datum::Record(expected.clone())]);
                        (effect.wait, Outcome::Completed(Datum::Null))
                    })
                    .collect()
            },
        );
        assert!(matches!(end, End::Finished(_)), "{end:?}");
        assert_eq!(calls, 1);
    }
    for call in [
        "display.show_message(text='hi', typo='bold')",
        "display.show_message(style='bold')",
        "display.show_message({'text': 'hi'}, text='again')",
    ] {
        let source = format!("async def demo():\n    return await {call}\n");
        let refused = crate::lower(&source, &environment, Mode::Module).expect_err(call);
        assert_eq!(refused.code, "PY_ARGUMENTS", "{call}: {refused}");
    }
}

/// The contract's refusals, under the codes every dialect shares.
#[test]
fn the_contract_refuses_under_the_shared_codes() {
    // FIG-7135: a code-only module still has no host-startable entry.
    for source in ["value = 3\n", ""] {
        let diagnostic = module(source).expect_err("a module must declare an entry");
        assert_eq!(diagnostic.code, "WORKFLOW_EMPTY_MODULE");
        assert_eq!(diagnostic.span, None);
        assert_eq!(
            diagnostic.kind,
            lash_kernel_dialect::DiagnosticKind::Refusal
        );
    }
    for (source, expected) in [
        (
            "async def a():\n    return 1\n\n\nasync def a():\n    return 2\n",
            ModuleRefusal::DuplicateEntry,
        ),
        (
            "limit = 3\n\n\nasync def a():\n    return limit\n",
            ModuleRefusal::CapturedModuleBinding,
        ),
        (
            "def helper():\n    return 1\n\n\nasync def a():\n    return helper()\n",
            ModuleRefusal::CapturedModuleBinding,
        ),
        (
            "async def b():\n    return 1\n\n\nasync def a():\n    return await b()\n",
            ModuleRefusal::CapturedModuleBinding,
        ),
        (
            "async def a(f: Callable[[], int]):\n    return 1\n",
            ModuleRefusal::UnmappableEntryAnnotation,
        ),
        (
            "async def a() -> \"Later\":\n    return 1\n",
            ModuleRefusal::UnmappableEntryAnnotation,
        ),
        (
            "async def a(limit: int = 3):\n    return limit\n",
            ModuleRefusal::EntryDefault,
        ),
    ] {
        assert_eq!(refusal(source), expected.code(), "{source}");
    }
}

/// ENTRY-CONV: host arguments bind the author's parameters directly;
/// calls in the body retain Python's positional and keyword ordering.
#[test]
fn entry_convention_keeps_python_positional_and_keyword_arguments() {
    use lash_kernel_doc::{Datum, Integer, StartConvention};

    let lowered = module("async def run(a: int, b: int):\n    async def pair(x, y):\n        return [x, y]\n    return [await pair(a, b), await pair(y=a, x=b)]\n").unwrap();
    let entry = super::linked::exported(&lowered.document, &Name::new("run"));
    assert_eq!(entry.convention, StartConvention::Direct);
    assert_eq!(
        lowered.document.functions[&entry.function]
            .signature
            .params
            .iter()
            .map(|param| param.name.clone())
            .collect::<Vec<_>>(),
        [Name::new("a"), Name::new("b")]
    );
    let a = Datum::Int(Integer::from(3));
    let b = Datum::Int(Integer::from(7));
    assert_eq!(
        machine::end_of_entry(lowered.document.into_document(), vec![a.clone(), b.clone()]),
        Datum::List(vec![
            Datum::List(vec![a.clone(), b.clone()]),
            Datum::List(vec![b, a])
        ])
    );
}

/// K-TASK-019: resolved task creation, including ensure_future in a callee,
/// installs cleanup at the run root: an entry's start convention declares
/// it, and a cell's root wraps it. Mere spelling and shadowed names do not.
#[test]
fn task_cleanup_follows_resolved_calls_and_belongs_to_the_entry_root() {
    let plain = module("# create_task gather\nasync def run():\n    def gather():\n        return 'create_task'\n    return gather()\n").unwrap();
    assert!(
        !plain
            .document
            .imports
            .values()
            .any(|import| import.name.package.to_string() == "tasks"
                && import.name
                    == lash_kernel_dialect::authoring::helper_name(
                        &lash_vocabulary::FunctionName::new("tasks.cancel_all").unwrap()
                    ))
    );
    let tasks = module("import asyncio\nasync def run():\n    async def child():\n        try:\n            await asyncio.sleep(1)\n        finally:\n            print('cleanup')\n    def launch():\n        return asyncio.ensure_future(child())\n    launch()\n    await asyncio.sleep(0)\n    print('done')\n    return 7\n").unwrap();
    let document = &tasks.document;
    let lash_kernel_doc::StartConvention::PyDirect { cleanup } =
        &super::linked::exported(document, &Name::new("run")).convention
    else {
        panic!("the entry declares no root cleanup");
    };
    assert_eq!(
        match cleanup {
            lash_kernel_doc::FunctionTarget::Import(slot) =>
                document.imports[slot].name.to_string(),
            lash_kernel_doc::FunctionTarget::Local(id) => document.functions[id].name.to_string(),
        },
        lash_kernel_dialect::authoring::helper_name(
            &lash_vocabulary::FunctionName::new("tasks.cancel_all").unwrap()
        )
        .to_string()
    );
    let no_try = |body: &[lash_kernel_doc::Stmt]| {
        !body
            .iter()
            .any(|stmt| matches!(stmt, lash_kernel_doc::Stmt::Try(_)))
    };
    assert!(
        no_try(
            &document.functions[&super::linked::exported(document, &Name::new("run")).function]
                .body()
                .unwrap()
                .block
        ) && no_try(document.main.as_deref().unwrap_or_default())
    );
    assert_eq!(
        machine::entry_output(tasks.document.into_document(), Vec::new()),
        (
            lash_kernel_doc::Datum::Int(lash_kernel_doc::Integer::from(7)),
            vec!["done".to_owned(), "cleanup".to_owned()]
        )
    );
    let (lines, end) = machine::output(
        "import asyncio\nasync def child():\n    try:\n        await asyncio.sleep(1)\n    finally:\n        print('cleanup')\nasyncio.ensure_future(child())\nawait asyncio.sleep(0)\nprint('done')\n",
    );
    assert_eq!(end, "ok");
    assert_eq!(lines, ["done", "cleanup"]);
}
