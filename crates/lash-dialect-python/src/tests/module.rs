//! MODULE-CONTRACT (FIG-6380): a Python workflow module under the workflow
//! contract every dialect shares. These cases pin what each Python
//! annotation means; the twin cases that compare entry names and refusal
//! codes with TypeScript run through the host's import.

use std::collections::BTreeMap;

use lash_kernel_dialect::workflow::ModuleRefusal;
use lash_kernel_dialect::{Environment, Lowered, Mode};
use lash_kernel_doc::{MapType, Name, Param, Signature, Type};

use super::machine;

fn module(source: &str) -> Result<Lowered, lash_kernel_dialect::Diagnostic> {
    let effects = machine::effects();
    let environment_namespaces =
        lash_kernel_dialect::Namespaces::of_signatures(&effects, machine::controls())
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

fn entries(source: &str) -> BTreeMap<Name, Signature> {
    module(source)
        .unwrap_or_else(|diagnostic| panic!("{diagnostic}\n{source}"))
        .document
        .entries
        .iter()
        .map(|(name, entry)| (name.clone(), entry.signature.clone()))
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

/// Every top-level `async def` is an entry. One defined inside a block, a
/// plain `def` and a nested `async def` are ordinary code.
#[test]
fn top_level_async_defs_are_the_entries_and_block_local_ones_are_not() {
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
    assert_eq!(
        entries(source).into_keys().collect::<Vec<_>>(),
        ["first", "second"].map(Name::new)
    );
}

/// A workflow names its tools with their namespaces, as the host's catalog
/// does (`display.show_message`): a module calls one by that dotted path,
/// keywords naming its parameters, as a cell calls a tool by its one name.
#[test]
fn a_module_calls_a_namespaced_tool_by_its_dotted_path() {
    let show = lash_kernel_doc::EffectName::new("display.show_message").expect("a tool's name");
    let effects = BTreeMap::from([(
        show.clone(),
        Signature {
            params: vec![param("text", Type::Text)],
            result: Type::Any,
        },
    )]);
    let environment_namespaces =
        lash_kernel_dialect::Namespaces::of_signatures(&effects, &Default::default())
            .expect("the test's effects are operations");
    let environment = Environment::new(machine::library(), &environment_namespaces);
    let lowered = crate::lower(
        "async def demo():\n    return await display.show_message(text=\"hi\")\n",
        &environment,
        Mode::Module,
    )
    .unwrap_or_else(|diagnostic| panic!("{diagnostic}"));
    assert!(lowered.document.manifest.effects.contains_key(&show));
}

/// FIG-6515: host tools take one input record. Python keywords name its
/// declared fields, and the performed argument stays that one record.
#[test]
fn tool_keywords_name_the_declared_input_fields() {
    use lash_kernel_doc::{Datum, EffectName};
    use lash_kernel_vm::{End, Outcome, Request};

    let show = EffectName::new("display.show_message").expect("a tool's name");
    let projected = |schema: serde_json::Value| {
        lash_kernel_check::type_of_schema(&schema).expect("a tool's input type")
    };
    let signature = lash_kernel_check::tool_signature(
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
        lash_kernel_doc::Type::Null,
    );
    let effects = BTreeMap::from([(show.clone(), signature.clone())]);
    let environment_namespaces =
        lash_kernel_dialect::Namespaces::of_signatures(&effects, machine::controls())
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
            lowered.document.manifest.effects[&show].signature,
            signature
        );
        let mut admission = lash_kernel_check::Environment::new(machine::library());
        admission.effects = effects.clone();
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
                        let Request::Effect(effect) = request else {
                            panic!("the tool call does not sleep");
                        };
                        calls += 1;
                        assert_eq!(effect.effect, cell_effect);
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
    for (source, expected) in [
        (
            "async def a():\n    return 1\n\n\nasync def a():\n    return 2\n",
            ModuleRefusal::DuplicateEntry,
        ),
        (
            "limit = 3\n\n\ndef helper():\n    return 1\n",
            ModuleRefusal::EmptyModule,
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
    let entry = &lowered.document.entries[&Name::new("run")];
    assert_eq!(entry.convention, StartConvention::Direct);
    assert_eq!(
        lowered.document.functions[&Name::new("run")].params,
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
            .manifest
            .functions
            .values()
            .any(|name| name.as_str() == "tasks.cancel_all")
    );
    let tasks = module("import asyncio\nasync def run():\n    async def child():\n        try:\n            await asyncio.sleep(1)\n        finally:\n            print('cleanup')\n    def launch():\n        return asyncio.ensure_future(child())\n    launch()\n    await asyncio.sleep(0)\n    print('done')\n    return 7\n").unwrap();
    let document = &tasks.document;
    let lash_kernel_doc::StartConvention::PyDirect { cleanup } =
        &document.entries[&Name::new("run")].convention
    else {
        panic!("the entry declares no root cleanup");
    };
    assert_eq!(
        document.manifest.functions[cleanup].as_str(),
        "tasks.cancel_all"
    );
    let no_try = |body: &[lash_kernel_doc::Stmt]| {
        !body
            .iter()
            .any(|stmt| matches!(stmt, lash_kernel_doc::Stmt::Try(_)))
    };
    assert!(no_try(&document.functions[&Name::new("run")].body) && no_try(&document.main));
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
