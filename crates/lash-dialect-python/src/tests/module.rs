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
        None,
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

/// Top-level sync and async defs are Exports; block and nested definitions
/// remain ordinary code. Each Export preserves its source async flag.
#[test]
fn top_level_function_exports_keep_async_flags_and_exclude_block_locals() {
    let source = "\
async def first() -> int:
    async def nested():
        return 0
    return await nested()


async def second(n: int) -> int:
    return n * 2


def helper() -> int:
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
        "async def demo() -> Any:\n    return await display.show_message(text=\"hi\")\n",
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
        let source = format!("async def demo() -> Any:\n    return await {call}\n");
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
        let source = format!("async def demo() -> Any:\n    return await {call}\n");
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
    let source = "async def a() -> int:\n    return 1\n\n\nasync def a() -> int:\n    return 2\n";
    assert_eq!(refusal(source), ModuleRefusal::DuplicateEntry.code());
}

/// ENTRY-CONV: host arguments bind the author's parameters directly;
/// calls in the body retain Python's positional and keyword ordering.
#[test]
fn entry_convention_keeps_python_positional_and_keyword_arguments() {
    use lash_kernel_doc::{Datum, Integer, StartConvention};

    let lowered = module("async def run(a: int, b: int) -> list[list[int]]:\n    async def pair(x, y):\n        return [x, y]\n    return [await pair(a, b), await pair(y=a, x=b)]\n").unwrap();
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
    let plain = module("# create_task gather\nasync def run() -> str:\n    def gather():\n        return 'create_task'\n    return gather()\n").unwrap();
    let cleanup_id =
        lash_kernel_dialect::Library::resolve(super::machine::library(), "tasks.cancel_all")
            .unwrap();
    assert!(!plain.document.library.contains(&cleanup_id));
    let tasks = module("import asyncio\nasync def run() -> int:\n    async def child():\n        try:\n            await asyncio.sleep(1)\n        finally:\n            print('cleanup')\n    def launch():\n        return asyncio.ensure_future(child())\n    launch()\n    await asyncio.sleep(0)\n    print('done')\n    return 7\n").unwrap();
    let document = &tasks.document;
    let lash_kernel_doc::StartConvention::PyDirect { cleanup } =
        &super::linked::exported(document, &Name::new("run")).convention
    else {
        panic!("the entry declares no root cleanup");
    };
    assert_eq!(cleanup, &lash_kernel_doc::FunctionTarget::Local(cleanup_id));
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

fn saved_handle(
    source: &str,
) -> (
    lash_kernel_doc::IdentifiedDocument,
    lash_kernel_doc::FunctionValue,
) {
    let lowered = machine::lower(source).expect("ordinary annotated function");
    let (site, closure) = lowered
        .written
        .keys()
        .find_map(|site| {
            let closure = lash_kernel_doc::snapshot::closure_at(
                &lowered.document,
                site,
                lash_vocabulary::LinkId::from_bytes([1; 32]),
                machine::installed().prepared.helpers(),
                &lash_kernel_doc::Selection::Fresh,
            )?;
            (lowered.written[site]
                .signature_origin
                .as_ref()?
                .name
                .as_deref()
                == Some("handle"))
            .then_some((site, closure))
        })
        .expect("the saved defining closure");
    let presentation = closure.presentation.as_ref().unwrap();
    let value = lash_kernel_doc::FunctionValue {
        written_signature: None,
        callable: lash_vocabulary::CallableId {
            link: lash_vocabulary::LinkId::from_bytes([1; 32]),
            function: closure.function,
        },
        captures: BTreeMap::new(),
        signature: presentation.signature.clone(),
        convention: presentation.convention.clone(),
        representation: presentation.representation.clone(),
        captions: lash_kernel_doc::Captions::empty(lowered.document.id()),
        creation: Some(site.clone()),
        token: None,
        signature_origin: lowered.written[site].signature_origin.clone(),
        implicit_text_conversions: lowered.written[site].implicit_text_conversions.clone(),
    };
    let wire = serde_json::to_string(&value).unwrap();
    let value: lash_kernel_doc::FunctionValue = serde_json::from_str(&wire).unwrap();
    (lowered.document, value)
}

/// FIG-7145: only entry promotion requires written parameter/result types;
/// ordinary definitions and lambdas remain valid with missing annotations.
#[test]
fn entry_promotion_requires_written_annotations_at_the_missing_position() {
    for (source, missing) in [
        (
            "async def handle(event) -> str:\n    return 'ok'\n",
            "event",
        ),
        ("def handle(event: Any):\n    return event\n", ")"),
    ] {
        machine::lower(source).expect("an ordinary function remains free");
        let error = module(source).expect_err("an entry needs a written signature");
        assert_eq!(error.code, "SIGNATURE_REQUIRED");
        let Some(lash_kernel_dialect::DiagnosticFact::SignatureRequired(required)) =
            error.fact.as_deref()
        else {
            panic!("a typed missing annotation: {error:?}");
        };
        assert_eq!(required.function.name.as_deref(), Some("handle"));
        assert_eq!(required.span, error.span);
        assert_eq!(
            required.position,
            if missing == "event" {
                lash_kernel_doc::SignaturePosition::Parameter {
                    index: 0,
                    name: Name::new("event"),
                }
            } else {
                lash_kernel_doc::SignaturePosition::Result
            }
        );
        let (document, value) = saved_handle(source);
        let lash_kernel_doc::FunctionValueRefusal::SignatureRequired { source: saved } = value
            .entry_export(
                &document,
                machine::installed().prepared.helpers(),
                &lash_kernel_doc::Selection::Fresh,
            )
            .expect_err("saved promotion needs annotations")
        else {
            panic!("typed saved refusal")
        };
        assert_eq!(saved.position, required.position);
        assert_eq!(saved.span, required.span);
        assert_eq!(saved.function.name, required.function.name);
        let span = error
            .span
            .expect("the missing annotation has a source position");
        let offset = source.find(missing).unwrap();
        if missing == "event" {
            assert_eq!(span.start, offset);
            assert_eq!(span.end, offset + missing.len());
        } else {
            assert_eq!(span.start, offset + 1);
            assert_eq!(span.end, offset + 1);
        }
    }
    machine::lower("identity = lambda value: value\n").expect("ordinary lambda");
}

/// FIG-7145: a saved defining closure and a module entry use the same
/// structural annotation contract, including explicitly open JSON data.
#[test]
fn typed_dict_entry_contract_survives_saved_function_promotion() {
    use lash_vocabulary::{RecordType, RecordTypeField};
    let source = "\
class Event(TypedDict, extra_items=JsonValue):
    actor: Actor
    values: list[tuple[int, str | None]]
    anything: NotRequired[object]
class Actor(TypedDict):
    name: str
    labels: list[Any]
async def handle(event: Event, metadata: dict[str, Any]) -> Event:
    return event
";
    let actor = Type::Record(RecordType {
        fields: vec![
            RecordTypeField {
                name: "name".into(),
                ty: Type::Text,
                optional: false,
            },
            RecordTypeField {
                name: "labels".into(),
                ty: Type::List(Box::new(Type::Json)),
                optional: false,
            },
        ],
        rest: lash_vocabulary::RecordRest::Closed,
    });
    let event = Type::Record(RecordType {
        fields: vec![
            RecordTypeField {
                name: "actor".into(),
                ty: actor,
                optional: false,
            },
            RecordTypeField {
                name: "values".into(),
                ty: Type::List(Box::new(Type::Tuple(vec![
                    Type::Int,
                    Type::Union(vec![Type::Text, Type::Null]),
                ]))),
                optional: false,
            },
            RecordTypeField {
                name: "anything".into(),
                ty: Type::Json,
                optional: true,
            },
        ],
        rest: lash_vocabulary::RecordRest::Keep(Box::new(Type::Json)),
    });
    let mut input_event = event.clone();
    if let Type::Record(record) = &mut input_event {
        let Type::Record(actor) = &mut record.fields[0].ty else {
            unreachable!()
        };
        actor.rest = lash_vocabulary::RecordRest::Drop;
    }
    let expected = Signature {
        params: vec![
            param("event", input_event),
            param("metadata", map(Type::Text, Type::Json)),
        ],
        result: event,
    };
    let (document, value) = saved_handle(source);
    assert_eq!(
        value
            .entry_export(
                &document,
                machine::installed().prepared.helpers(),
                &lash_kernel_doc::Selection::Fresh
            )
            .unwrap()
            .signature,
        expected
    );
    assert_eq!(entries(source)[&Name::new("handle")], expected);
    assert_eq!(
        entries("def handle(value: Any) -> object:\n    return value\n")[&Name::new("handle")],
        Signature {
            params: vec![param("value", Type::Json)],
            result: Type::Json
        }
    );
    use lash_vocabulary::AnnotationResolutionReason as Reason;
    for (source, reason, annotation) in [
        (
            "class Loop(TypedDict):\n    next: Loop\ndef handle(value: Loop) -> None:\n    pass\n",
            Reason::Cycle {
                name: "Loop".into(),
            },
            "Loop)",
        ),
        (
            "def handle(value: Callable[[], int]) -> int:\n    return 1\n",
            Reason::Unsupported {
                construct: "Callable".into(),
            },
            "Callable[[], int]",
        ),
        (
            "def handle(value: Missing) -> int:\n    return 1\n",
            Reason::Unresolved {
                name: "Missing".into(),
            },
            "Missing",
        ),
        (
            "def handle(value: int) -> \"Later\":\n    return 1\n",
            Reason::Unsupported {
                construct: "annotation".into(),
            },
            "\"Later\"",
        ),
    ] {
        let error = module(source).expect_err("unrepresentable entry annotation");
        assert_eq!(error.code, "ENTRY_ANNOTATION_REFUSED");
        assert!(error.message.is_empty());
        let Some(lash_kernel_dialect::DiagnosticFact::EntryAnnotationRefused(refused)) =
            error.fact.as_deref()
        else {
            panic!("typed resolution fact")
        };
        assert_eq!(refused.reason, reason);
        assert_eq!(refused.function.name.as_deref(), Some("handle"));
        let start = source.find(annotation).unwrap();
        let width = if annotation == "Loop)" {
            4
        } else {
            annotation.len()
        };
        assert_eq!(
            refused.span,
            Some(lash_vocabulary::sites::Span {
                start,
                end: start + width
            })
        );
        let (document, value) = saved_handle(source);
        let lash_kernel_doc::FunctionValueRefusal::EntryAnnotation { source: saved } = value
            .entry_export(
                &document,
                machine::installed().prepared.helpers(),
                &lash_kernel_doc::Selection::Fresh,
            )
            .expect_err("same saved refusal")
        else {
            panic!("typed saved resolution fact")
        };
        assert_eq!(saved.reason, refused.reason);
        assert_eq!(saved.position, refused.position);
        assert_eq!(saved.span, refused.span);
    }
    // Expansion has a host-configurable budget, independently of source size.
    let effects = machine::effects();
    let namespaces = crate::tests::linked::namespaces(&effects, machine::controls()).unwrap();
    let environment = Environment::new(machine::library(), &namespaces);
    let error = crate::lower_with_limits(
        source,
        &environment,
        Mode::Module,
        crate::SourceLimits::standard().with_annotation_nodes(1),
    )
    .expect_err("bounded expansion");
    let Some(lash_kernel_dialect::DiagnosticFact::EntryAnnotationRefused(refused)) =
        error.fact.as_deref()
    else {
        panic!("typed expansion limit")
    };
    assert_eq!(refused.reason, Reason::Limit);
}

/// FIG-7189: an open record's undeclared fields are positional input,
/// never keyword parameters that the operation contract does not declare.
#[test]
fn open_record_operation_inputs_print_as_positional_records() {
    let signature = lash_vocabulary::schema::tool_signature(
        Type::Record(lash_vocabulary::RecordType {
            fields: Vec::new(),
            rest: lash_vocabulary::RecordRest::Keep(Box::new(Type::Any)),
        }),
        false,
        Type::Any,
    );
    let effects = BTreeMap::from([(
        lash_vocabulary::EffectName::new("host.echo").unwrap(),
        signature,
    )]);
    let namespaces = super::linked::namespaces(&effects, &BTreeMap::new()).unwrap();
    let environment = Environment::new(machine::library(), &namespaces);
    let source = "async def handle(x: int) -> Any:\n    return await host.echo({\"value\": x, \"extra-key\": 1})\n";
    let lowered = crate::lower(source, &environment, Mode::Module).unwrap();
    let printed = crate::print(
        &lowered.document,
        None,
        machine::library(),
        Default::default(),
    )
    .unwrap();
    assert!(printed.source.contains("host.echo({"), "{}", printed.source);
    let (read, _) = crate::read(
        &printed.source,
        &lowered.document,
        machine::library(),
        Default::default(),
        None,
    )
    .unwrap();
    assert_eq!(read.canonical_bytes(), lowered.document.canonical_bytes());
}

/// ENTRY-RECORD-MODES: the same TypedDict resolves at each boundary position,
/// with its input mode preserved by source printing and saved promotion.
#[test]
fn entry_record_modes_survive_source_and_saved_function_promotion() {
    use lash_vocabulary::RecordRest;
    for (suffix, rest, result_rest) in [
        ("", RecordRest::Drop, RecordRest::Closed),
        (", closed=True", RecordRest::Closed, RecordRest::Closed),
        (
            ", extra_items=JsonValue",
            RecordRest::Keep(Box::new(Type::Json)),
            RecordRest::Keep(Box::new(Type::Json)),
        ),
    ] {
        let source = format!(
            "class Event(TypedDict{suffix}):\n    name: str\ndef handle(event: Event) -> Event:\n    return event\n"
        );
        let signature = entries(&source).remove(&Name::new("handle")).unwrap();
        let Type::Record(input) = &signature.params[0].ty else {
            panic!("record input")
        };
        let Type::Record(result) = &signature.result else {
            panic!("record result")
        };
        assert_eq!(input.rest, rest);
        assert_eq!(result.rest, result_rest);
        let lowered = module(&source).unwrap();
        let printed = crate::print(
            &lowered.document,
            Some(&lowered.captions),
            machine::library(),
            Default::default(),
        )
        .unwrap();
        assert_eq!(entries(&printed.source)[&Name::new("handle")], signature);
        let (document, value) = saved_handle(&source);
        assert_eq!(
            value
                .entry_export(
                    &document,
                    machine::installed().prepared.helpers(),
                    &lash_kernel_doc::Selection::Fresh
                )
                .unwrap()
                .signature,
            signature
        );
    }
}

/// K-FN-002: helpers bind module constants, even under parameter shadowing.
#[test]
fn materialized_entry_has_no_free_module_bindings() {
    let source = "base = 7\ndef helper(x: int) -> int:\n    global base\n    return x + base\nasync def run(base: int) -> int:\n    return helper(x=base)\n";
    let lowered = module(source).expect("static module bindings materialize");
    assert_eq!(
        machine::end_of_entry(
            lowered.document.into_document(),
            vec![lash_kernel_doc::Datum::Int(3.into())]
        ),
        lash_kernel_doc::Datum::Int(10.into())
    );
    let namespace = module(&source.replace(
        "return helper(x=base)",
        "return helper(x=base) + globals()['base']",
    ))
    .expect("materialized guest namespace");
    assert_eq!(
        machine::end_of_entry(
            namespace.document.into_document(),
            vec![lash_kernel_doc::Datum::Int(3.into())]
        ),
        lash_kernel_doc::Datum::Int(17.into())
    );
    assert_eq!(
        refusal(
            "values = []\nvalues.append(1)\nasync def run() -> list[int]:\n    return values\n"
        ),
        ModuleRefusal::DynamicModuleInitialization.code()
    );
}

/// ENTRY-DEFAULT: absent selects defaults; None remains an authored argument.
#[test]
fn entry_defaults_test_absence_in_definition_order() {
    let source = "seen = []\ndef pick(label: str, value: int) -> int:\n    seen.append(label)\n    return value\nasync def run(a: int | None = pick('a', 3), b: int = pick('b', 4)) -> list[Any]:\n    return [seen, a, b]\n";
    for source in [
        source.to_owned(),
        source.replace(
            "a: int | None = pick('a', 3), b: int = pick('b', 4)",
            "a: int | None = pick('a', 3), /, *, b: int = pick('b', 4)",
        ),
    ] {
        for (args, expected) in [
            (
                vec![],
                vec![
                    lash_kernel_doc::Datum::List(vec![
                        lash_kernel_doc::Datum::Text("a".into()),
                        lash_kernel_doc::Datum::Text("b".into()),
                    ]),
                    lash_kernel_doc::Datum::Int(3.into()),
                    lash_kernel_doc::Datum::Int(4.into()),
                ],
            ),
            (
                vec![lash_kernel_doc::Datum::Null],
                vec![
                    lash_kernel_doc::Datum::List(vec![lash_kernel_doc::Datum::Text("b".into())]),
                    lash_kernel_doc::Datum::Null,
                    lash_kernel_doc::Datum::Int(4.into()),
                ],
            ),
        ] {
            let lowered = module(&source).expect("entry defaults lower");
            assert_eq!(
                machine::end_of_entry(lowered.document.into_document(), args),
                lash_kernel_doc::Datum::List(expected)
            );
        }
    }
}

/// ENTRY-STRUCTURAL: finite aliases and text Literal alternatives stay exact.
#[test]
fn finite_aliases_map_to_the_exact_boundary_shape() {
    let source = "from typing import Literal, TypeAlias\nChoice: TypeAlias = Literal['yes', 'no']\nAlias = Literal[Choice]\nFlag = Literal[True, False]\nasync def flag(x: Flag) -> Flag:\n    return x\nasync def run(x: Choice) -> Alias:\n    return x\n";
    let signature = entries(source).remove(&Name::new("run")).unwrap();
    assert_eq!(
        signature.params[0].ty,
        Type::Enum(vec!["yes".into(), "no".into()])
    );
    assert_eq!(signature.result, signature.params[0].ty);
    let flag = entries(source).remove(&Name::new("flag")).unwrap();
    assert_eq!(flag.params[0].ty, Type::Bool);
    assert_eq!(flag.result, Type::Bool);

    assert_eq!(
        machine::output("Literal = [7]\nAlias = Literal[0]\nprint(Alias)\n").0,
        ["7"]
    );

    assert_eq!(
        machine::session(&["Literal = [7]", "Alias = Literal[0]\nprint(Alias)"]),
        ["7"]
    );
    assert_eq!(machine::output("from typing import TypeAlias, Literal\nChoice: TypeAlias = Literal['yes']\nChoice: int = 7\nprint(Choice)\n").0, ["7"]);
}

/// KD14 / K-CLO-004 / K-SES-003: names are local to each captured environment;
/// two instances stay distinct and an alias still names the shared instance.
#[test]
fn capture_hygiene_preserves_shared_and_distinct_cells() {
    use lash_kernel_doc::{CaptureValue, Datum};
    let source = "def make(value):\n    def read() -> int:\n        return value\n    return read\nleft = make(1)\nright = make(2)\nalias = left\ndef run() -> list[int]:\n    return [left(), right(), alias()]\n";
    let mut crossed = false;
    let (_, end) = machine::drive(
        &format!("{source}await host.echo(run)\n"),
        machine::effects(),
        super::pages::Session::default(),
        |_, park, _| {
            let lash_kernel_vm::Request::Effect(effect) = &park.requests[0];
            let Datum::FunctionValue(function) = &effect.args[0] else {
                panic!("callable crosses");
            };
            let nodes = function.nodes();
            assert_eq!(nodes.len(), 3);
            assert_eq!(
                nodes[1].captures[&Name::new("value")],
                CaptureValue::Data(Datum::Int(1.into()))
            );
            assert_eq!(
                nodes[2].captures[&Name::new("value")],
                CaptureValue::Data(Datum::Int(2.into()))
            );
            assert_eq!(
                function.captures[&Name::new("left")],
                CaptureValue::Reference(1)
            );
            crossed = true;
            vec![(effect.wait, lash_kernel_vm::Outcome::Completed(Datum::Null))]
        },
    );
    assert!(crossed && matches!(end, lash_kernel_vm::End::Finished(_)));
}

/// K-CAP-002: callable adapters and exception tokens preserve the authored
/// definition, catch clause and captions through exact source readback.
#[test]
fn shared_callable_bodies_and_exception_tokens_keep_caption_readback() {
    let source = "#: Run the helper\nasync def run(value: str) -> Any:\n    #: Define the helper\n    def echo(item):\n        #: Return the item\n        return item\n    saved = echo\n    error_class = ValueError\n    #: Try it\n    try:\n        result = echo(value)\n    #: Recover\n    except (error_class, Exception):\n        result = 'caught'\n    #: Always finish\n    finally:\n        result = result + '!'\n    return result\n";
    let lowered = module(source).expect("callable and catch module");
    let printed = crate::print(
        &lowered.document,
        Some(&lowered.captions),
        machine::library(),
        lash_kernel_dialect::PrintOptions {
            source_map: true,
            ..Default::default()
        },
    )
    .expect("authored callable and catch spelling");
    let (read, captions) = crate::read(
        &printed.source,
        &lowered.document,
        machine::library(),
        Default::default(),
        None,
    )
    .expect("authored callable and catch readback");
    assert_eq!(read.canonical_bytes(), lowered.document.canonical_bytes());
    assert_eq!(captions, lowered.captions);
    printed
        .source_map
        .expect("printed map")
        .validate(&read, &printed.source)
        .expect("map matches exact readback");
}

/// K-CAP-002: assigning a lambda keeps its anonymous callable identity;
/// spelling it as a named def changes the binder and direct-call proof.
#[test]
fn assigned_lambdas_keep_their_callable_identity_when_printed() {
    let source = "async def run(flag: bool) -> Any:\n    check = lambda value: value if flag else None\n    return check('x')\n";
    let lowered = module(source).expect("assigned lambda module");
    let printed = crate::print(
        &lowered.document,
        Some(&lowered.captions),
        machine::library(),
        Default::default(),
    )
    .expect("assigned lambda spelling");
    let (read, captions) = crate::read(
        &printed.source,
        &lowered.document,
        machine::library(),
        Default::default(),
        None,
    )
    .expect("assigned lambda readback");
    assert_eq!(read.canonical_bytes(), lowered.document.canonical_bytes());
    assert_eq!(captions, lowered.captions);
}

/// K-CAP-002: a private iterator slot cannot become a guest namespace entry.
#[test]
fn for_bindings_keep_the_guest_namespace_when_printed() {
    let source = "async def run(items: list[str]) -> Any:\n    for item in items:\n        pass\n    check = lambda value: value\n    return check(item)\n";
    let lowered = module(source).expect("loop and dynamic call");
    let printed = crate::print(
        &lowered.document,
        Some(&lowered.captions),
        machine::library(),
        lash_kernel_dialect::PrintOptions {
            source_map: true,
            ..Default::default()
        },
    )
    .expect("authored loop binding");
    let (read, captions) = crate::read(
        &printed.source,
        &lowered.document,
        machine::library(),
        Default::default(),
        None,
    )
    .expect("loop binding readback");
    assert_eq!(read.canonical_bytes(), lowered.document.canonical_bytes());
    assert_eq!(captions, lowered.captions);
    printed
        .source_map
        .expect("printed map")
        .validate(&read, &printed.source)
        .expect("loop binding map");
}

/// FIG-7352: a saved callable's packed token has an ordinary Python spelling.
#[test]
fn materialized_saved_callable_tokens_reopen_as_ordinary_python() {
    use lash_kernel_doc::{CaptureValue, Datum, Expr, Literal, Node};
    fn data(expr: &Expr, callable: &Name) -> Datum {
        match expr {
            Expr::Literal(Literal::Null) => Datum::Null,
            Expr::Literal(Literal::Bool(value)) => Datum::Bool(*value),
            Expr::Literal(Literal::Int(value)) => Datum::Int(value.clone()),
            Expr::Literal(Literal::Text(value)) => Datum::Text(value.clone()),
            Expr::Variable(name) if name == callable => Datum::Null,
            Expr::Tuple(parts) => {
                Datum::Tuple(parts.iter().map(|part| data(part, callable)).collect())
            }
            _ => panic!("literal token data: {expr:?}"),
        }
    }
    let lowered =
        machine::lower("offset = 7\nasync def handle(x: int) -> int:\n    return x + offset\n")
            .unwrap();
    let (site, written) = lowered
        .written
        .iter()
        .find(|(_, written)| {
            written
                .signature_origin
                .as_ref()
                .is_some_and(|origin| origin.name.as_deref() == Some("handle"))
        })
        .unwrap();
    let Node::Expr(Expr::Closure(closure)) = lowered.document.node(site).unwrap() else {
        panic!("saved closure")
    };
    let presentation = closure.presentation.as_ref().unwrap();
    let mut parent = site.clone();
    parent.path.pop().unwrap();
    let Node::Expr(Expr::Let { name, body, .. }) = lowered.document.node(&parent).unwrap() else {
        panic!("callable binding")
    };
    let Datum::Tuple(token) = data(body, name) else {
        panic!("callable token")
    };
    let document = lowered.document.document().clone();
    let link = lash_vocabulary::Link {
        document: document.identity(),
        fills: Default::default(),
    };
    let function = lash_kernel_doc::FunctionValue {
        written_signature: None,
        signature_origin: written.signature_origin.clone(),
        implicit_text_conversions: written.implicit_text_conversions.clone(),
        callable: lash_vocabulary::CallableId {
            link: link.identity(),
            function: closure.function,
        },
        captures: [(
            Name::new("offset"),
            CaptureValue::Data(Datum::Int(7.into())),
        )]
        .into(),
        signature: presentation.signature.clone(),
        convention: presentation.convention.clone(),
        representation: presentation.representation.clone(),
        captions: lowered.captions,
        creation: Some(site.clone()),
        token: Some(token),
    };
    let content = lash_kernel_package::LinkedContent {
        documents: [(document.identity(), document)].into(),
        links: [(link.identity(), link)].into(),
        ..Default::default()
    };
    let opened = lash_kernel_package::materialize_workflow(
        &content,
        &lash_kernel_doc::WorkflowReference::Callable(function.callable),
        Some(&function),
        None,
        machine::installed().prepared.helpers(),
    )
    .unwrap();
    crate::print(
        &opened.document,
        None,
        machine::library(),
        Default::default(),
    )
    .expect("materialized callable has an exact authored Python readback");
}
