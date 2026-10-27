//! FIG-6893: a host names a workflow's values in plain words. A source
//! binding's caption is its declaring statement's, joined onto the binding
//! and kept through print and read in TypeScript and Python, and a binding
//! that holds an awaited tool call's reply names that `perform`.
use lash::dialect::SourceFormat;
use lash::workflow::WorkflowDocument;
use lash::workflow::document::{self as k, Captions, Document};
use lash::workflow::view::{BindingView, SiteKind, WorkflowView, derive_callable};
use std::collections::BTreeMap;

const TYPESCRIPT: &str = r#"export async function route(priority: string): any {
  /** The filed request */
  const reply = await inbox.sendItem({ title: priority });
  const pending = inbox.sendItem({ title: "later" });
  const later = await pending;
  const urgent = (await inbox.sendItem({ title: "check" })).urgent;
  /** The marked priority */
  const marked = priority + "!";
  if (reply === marked) { await inbox.sendItem({ title: "2" }); }
  return [later, urgent];
}"#;

const PYTHON: &str = "async def route(priority: str) -> list[Any]:
    #: The filed request
    reply = await inbox.send_item({\"title\": priority})
    pending = asyncio.create_task(inbox.send_item({\"title\": \"later\"}))
    later = await pending
    urgent = (await inbox.send_item({\"title\": \"check\"}))[\"urgent\"]
    #: The marked priority
    marked = priority + \"!\"
    if reply == marked:
        await inbox.send_item({\"title\": \"2\"})
    return [later, urgent]
";

/// `source` lowered as a module against one `inbox.send_item` tool, with
/// the captions it spells.
fn lower(format: SourceFormat, source: &str) -> (Document, Captions) {
    let embedding = lash_vm_worker::built_embedding(&Default::default()).expect("embedding");
    let effects = BTreeMap::from([(
        k::EffectName::new("inbox.send_item").expect("a tool's name"),
        lash::schema::Signature {
            params: vec![lash::schema::Param {
                name: lash::schema::Name::new("input"),
                ty: lash::schema::Type::Any,
                optional: false,
            }],
            result: lash::schema::Type::Any,
        },
    )]);
    let environment_namespaces = super::namespaces(effects);
    let environment = lash::dialect::Environment::new(embedding.library(), &environment_namespaces);
    let mode = lash::dialect::Mode::Module;
    let lowered = match format {
        SourceFormat::TypeScriptV1 => {
            lash::typescript::lower(source, &environment, mode).expect("lower TypeScript")
        }
        SourceFormat::PythonV1 => {
            lash::python::lower(source, &environment, mode).expect("lower Python")
        }
    };
    (lowered.document.into_document(), lowered.captions)
}

/// The checked holder and view of `document` entered at `route`.
fn view(document: Document) -> (WorkflowDocument, WorkflowView) {
    let unit = super::export_unit(&document, "route");
    let (holder, context) = super::holder(document);
    let view = derive_callable(&holder, &unit, &context).expect("view");
    (holder, view)
}

fn binding<'v>(view: &'v WorkflowView, name: &str) -> &'v BindingView {
    view.dataflow
        .bindings
        .iter()
        .find(|binding| binding.name.as_str() == name)
        .unwrap_or_else(|| panic!("a binding `{name}`: {:#?}", view.dataflow.bindings))
}

/// Each captioned binding's name and title.
fn binding_captions(
    holder: &WorkflowDocument,
    view: &WorkflowView,
    captions: &Captions,
) -> Vec<(String, String)> {
    view.captions(holder, captions)
        .expect("the captions join the view")
        .bindings
        .iter()
        .map(|caption| {
            let declared = view
                .dataflow
                .bindings
                .iter()
                .find(|binding| binding.id == caption.binding)
                .expect("a caption names a binding of the view");
            assert_eq!(
                (&declared.name, &declared.site),
                (&caption.name, &caption.site),
                "a binding caption is its declaring statement's"
            );
            (
                caption.name.as_str().to_owned(),
                caption.caption.title().to_owned(),
            )
        })
        .collect()
}

/// The caption a source spells on a declaration names the binding it
/// declares, for a reply and for code alike; printing the document with its
/// captions and reading the source back keeps each at its binding. No
/// variable the lowering declares is named.
#[test]
fn a_binding_caption_round_trips_through_print_and_read_in_typescript_and_python() {
    let embedding = lash_vm_worker::built_embedding(&Default::default()).expect("embedding");
    let expected = [
        ("reply".to_owned(), "The filed request".to_owned()),
        ("marked".to_owned(), "The marked priority".to_owned()),
    ];
    for (format, source) in [
        (SourceFormat::TypeScriptV1, TYPESCRIPT),
        (SourceFormat::PythonV1, PYTHON),
    ] {
        let (document, captions) = lower(format, source);
        let (holder, authored) = view(document.clone());
        assert_eq!(
            binding_captions(&holder, &authored, &captions),
            expected,
            "{format:?} authored:\n{}",
            k::print_document(holder.document())
        );
        let options = Default::default();
        let printed = match format {
            SourceFormat::TypeScriptV1 => {
                lash::typescript::print(&document, Some(&captions), embedding.library(), options)
            }
            SourceFormat::PythonV1 => {
                lash::python::print(&document, Some(&captions), embedding.library(), options)
            }
        }
        .unwrap_or_else(|error| panic!("{format:?} print with captions: {error:?}"));
        assert!(printed.dropped.is_empty(), "{format:?} drops no caption");
        let (read, read_captions) = match format {
            SourceFormat::TypeScriptV1 => lash::typescript::read(
                &printed.source,
                &document,
                embedding.library(),
                Default::default(),
                None,
            )
            .expect("read TypeScript back"),
            SourceFormat::PythonV1 => lash::python::read(
                &printed.source,
                &document,
                embedding.library(),
                Default::default(),
                None,
            )
            .expect("read Python back"),
        };
        assert_eq!(
            read.canonical_bytes(),
            document.canonical_bytes(),
            "{format:?}"
        );
        let (holder, reread) = view(read);
        assert_eq!(
            binding_captions(&holder, &reread, &read_captions),
            expected,
            "{format:?} read back:\n{}",
            printed.source
        );
    }
}

/// A binding that holds an awaited tool call's reply names that `perform`,
/// awaited where it stands or through a promise or task that only performs
/// it; a field read from a reply and a value of code name none.
#[test]
fn produced_by_names_the_perform_a_binding_awaits() {
    for (format, source) in [
        (SourceFormat::TypeScriptV1, TYPESCRIPT),
        (SourceFormat::PythonV1, PYTHON),
    ] {
        let (holder, view) = view(lower(format, source).0);
        let code = k::print_document(holder.document());
        let performs: Vec<_> = view
            .dataflow
            .actions
            .iter()
            .filter(|action| action.kind == SiteKind::Perform)
            .map(|action| action.site.clone())
            .collect();
        let produced = |name: &str| binding(&view, name).produced_by.clone();
        assert_eq!(
            [produced("reply"), produced("later")],
            [Some(performs[0].clone()), Some(performs[1].clone())],
            "{format:?}: reply's perform, then the one the pending task runs:\n{code}"
        );
        assert_eq!(
            [produced("urgent"), produced("marked"), produced("priority")],
            [None, None, None],
            "{format:?}: a field of a reply, code and a parameter:\n{code}"
        );
    }
}
