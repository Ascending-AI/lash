//! MODULE-CONTRACT (FIG-6380): a Python workflow module under the workflow
//! contract every dialect shares. These cases pin what each Python
//! annotation means; the twin cases that compare entry names and refusal
//! codes with TypeScript run through the host's import.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_dialect::workflow::ModuleRefusal;
use lash_kernel_dialect::{Environment, Lowered, Mode};
use lash_kernel_doc::{MapType, Name, Param, Signature, Type};

use super::machine;

fn module(source: &str) -> Result<Lowered, lash_kernel_dialect::Diagnostic> {
    let effects = machine::effects();
    let environment = Environment {
        library: machine::library(),
        effects: &effects,
        controls: machine::controls(),
        tool_roots: &BTreeSet::new(),
    };
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
    let environment = Environment {
        library: machine::library(),
        effects: &effects,
        controls: &BTreeMap::new(),
        tool_roots: &BTreeSet::from([Name::new("display")]),
    };
    let lowered = crate::lower(
        "async def demo():\n    return await display.show_message(text=\"hi\")\n",
        &environment,
        Mode::Module,
    )
    .unwrap_or_else(|diagnostic| panic!("{diagnostic}"));
    assert!(lowered.document.manifest.effects.contains_key(&show));
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
