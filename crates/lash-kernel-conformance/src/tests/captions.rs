//! The `K-CAP` laws: each rule's shard runs the generic caption harness for
//! every dialect, and the rules no dialect takes part in (rekeying and the
//! text domain) once, over kernel documents.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_dialect::{Dialect, EffectControl, Environment, Mode, NamedLibrary, SourceFormat};
use lash_kernel_doc::ActKind;
use lash_kernel_doc::Action;
use lash_kernel_doc::Caption;
use lash_kernel_doc::CaptionEntry;
use lash_kernel_doc::CaptionField;
use lash_kernel_doc::CaptionKind;
use lash_kernel_doc::CaptionRefusal;
use lash_kernel_doc::CaptionTarget;
use lash_kernel_doc::Captions;
use lash_kernel_doc::CaptionsRefusal;
use lash_kernel_doc::Document;
use lash_kernel_doc::Expr;
use lash_kernel_doc::Literal;
use lash_kernel_doc::Node;
use lash_kernel_doc::Stmt;
use lash_kernel_doc::parse_document;
use lash_kernel_edit::authoring::{Branch, DataValue, Operation, ValueTarget, Wrapper};
use lash_kernel_edit::{Draft, Position, Transaction};
use lash_vocabulary::names::EffectName;
use lash_vocabulary::names::Name;
use lash_vocabulary::sites::Site;
use lash_vocabulary::sites::Unit;
use lash_vocabulary::types::Param;
use lash_vocabulary::types::Signature;
use lash_vocabulary::types::Type;

use crate::captions::{CaptionCorpus, CaptionDialect};

const TYPESCRIPT: &str = include_str!("../../captions/typescript.json");
const PYTHON: &str = include_str!("../../captions/python.json");

/// The rule's laws, for every dialect where a dialect takes part.
pub(super) fn verify(rule: &str) {
    match rule {
        "K-CAP-004" => {
            rekey();
            each_dialect(rekey_imported);
            each_dialect(rekey_canvas_edits);
        }
        "K-CAP-005" => {
            text_domain();
            each_dialect(|dialect| dialect.printer_validates());
        }
        "K-CAP-001" => each_dialect(|dialect| dialect.behaviour_free()),
        "K-CAP-002" => each_dialect(|dialect| dialect.readback()),
        "K-CAP-003" => each_dialect(|dialect| dialect.authored()),
        "K-CAP-006" => each_dialect(|dialect| dialect.dropped()),
        "K-CAP-007" => {
            signature_targets();
            each_dialect(signature_view);
        }
        _ => unreachable!("{rule} is not a caption rule"),
    }
}

fn each_dialect(law: impl Fn(&CaptionDialect<'_>) -> Result<(), crate::HarnessError>) {
    let effects = effects();
    let controls: BTreeMap<EffectName, BTreeSet<EffectControl>> = BTreeMap::new();
    let dialects: [(Dialect, NamedLibrary, SourceFormat, &str); 2] = [
        (
            typescript(),
            library(lash_ext_helper_compiler::library::Dialect::TypeScript),
            SourceFormat::TypeScriptV1,
            TYPESCRIPT,
        ),
        (
            lash_dialect_python::dialect(),
            library(lash_ext_helper_compiler::library::Dialect::Python),
            SourceFormat::PythonV1,
            PYTHON,
        ),
    ];
    let namespaces =
        crate::linked::namespaces(&effects, &controls).expect("test operation namespaces");
    for (package, library, format, corpus) in &dialects {
        let corpus = CaptionCorpus::from_json(corpus).expect("caption corpus");
        let dialect = CaptionDialect {
            package,
            environment: Environment::new(library, &namespaces),
            format: *format,
            corpus: &corpus,
        };
        law(&dialect).unwrap_or_else(|error| panic!("{}: {error}", package.name));
    }
}

fn typescript() -> Dialect {
    Dialect {
        name: "typescript".into(),
        front_end: Box::new(lash_dialect_typescript::TypeScript::default()),
        read: |source, base, library, limits| {
            lash_dialect_typescript::read(source, base, library, limits).map_err(Into::into)
        },
        printer: Some(Box::new(lash_dialect_typescript::TypeScript::default())),
        captions: lash_dialect_typescript::CAPTIONS,
        function_values: Some(lash_dialect_typescript::function_values()),
    }
}

fn library(dialect: lash_ext_helper_compiler::library::Dialect) -> NamedLibrary {
    let built = lash_ext_helper_compiler::library::Library::of(dialect).expect("built packages");
    NamedLibrary::from_registry(&built.registry).expect("unique package names")
}

fn effects() -> BTreeMap<EffectName, Signature> {
    BTreeMap::from([(
        EffectName::new("corpus.echo").expect("a tool's name"),
        Signature {
            params: vec![Param {
                name: Name::new("input"),
                ty: Type::Any,
                optional: false,
            }],
            result: Type::Any,
        },
    )])
}

fn caption(title: &str) -> Caption {
    Caption::new(title, None).expect("a caption")
}

fn main_site(path: &[u32]) -> Site {
    Site::new(Unit::Main, path.to_vec())
}

/// `K-CAP-005`: every refusal of the text domain and of a captions value,
/// the boundaries it admits, and decoding that checks the same.
fn text_domain() {
    use CaptionField::{Description, Title};
    let refused = |title: &str, description: Option<&str>| {
        Caption::new(title, description.map(str::to_owned)).expect_err(title)
    };
    let long_title = "t".repeat(201);
    let long_description = "d".repeat(2049);
    let cases: [(&str, Option<&str>, CaptionRefusal); 21] = [
        ("", None, CaptionRefusal::EmptyTitle),
        ("Send", Some(""), CaptionRefusal::EmptyDescription),
        (
            &long_title,
            None,
            CaptionRefusal::TooLong {
                field: Title,
                limit: 200,
            },
        ),
        (
            "Send",
            Some(&long_description),
            CaptionRefusal::TooLong {
                field: Description,
                limit: 2048,
            },
        ),
        (
            "a\rb",
            None,
            CaptionRefusal::LineSeparator {
                field: Title,
                character: '\r',
            },
        ),
        (
            "Send",
            Some("a\u{2028}b"),
            CaptionRefusal::LineSeparator {
                field: Description,
                character: '\u{2028}',
            },
        ),
        (
            "Send",
            Some("a\u{2029}b"),
            CaptionRefusal::LineSeparator {
                field: Description,
                character: '\u{2029}',
            },
        ),
        (
            "a\tb",
            None,
            CaptionRefusal::Control {
                field: Title,
                character: '\t',
            },
        ),
        (
            "Send",
            Some("a\u{0}b"),
            CaptionRefusal::Control {
                field: Description,
                character: '\u{0}',
            },
        ),
        ("a\nb", None, CaptionRefusal::MultilineTitle),
        (" Send", None, CaptionRefusal::Untrimmed { field: Title }),
        (
            "Send",
            Some("a \nb"),
            CaptionRefusal::Untrimmed { field: Description },
        ),
        ("Send", Some("\na"), CaptionRefusal::BlankEdge),
        ("Send", Some("a\n"), CaptionRefusal::BlankEdge),
        ("Send", Some("a\n\n\nb"), CaptionRefusal::BlankRun),
        (
            "Done */ await wipe(); /*",
            None,
            CaptionRefusal::CommentClose { field: Title },
        ),
        (
            "Send",
            Some("Done */ await wipe(); /*"),
            CaptionRefusal::CommentClose { field: Description },
        ),
        (
            "@label Send",
            None,
            CaptionRefusal::TagLine { field: Title },
        ),
        (
            "Send",
            Some("To the team\n@param to"),
            CaptionRefusal::TagLine { field: Description },
        ),
        // A title's first check wins over its description's.
        ("", Some(""), CaptionRefusal::EmptyTitle),
        ("a\nb", Some("@x"), CaptionRefusal::MultilineTitle),
    ];
    for (title, description, expected) in cases {
        assert_eq!(
            refused(title, description),
            expected,
            "{title:?} {description:?}"
        );
        // Decoding is the same gate: no unchecked caption exists.
        let json = serde_json::json!({"title": title, "description": description});
        assert!(
            serde_json::from_value::<Caption>(json).is_err(),
            "{title:?}"
        );
    }
    for (title, description) in admitted() {
        let accepted = Caption::new(title.clone(), description.clone()).expect(&title);
        let json = serde_json::to_value(&accepted).expect("encodes");
        assert_eq!(
            serde_json::from_value::<Caption>(json).expect("decodes"),
            accepted
        );
    }

    let document = parse_document(
        "kernel 1\nnumbers float\nfn helper() { return null }\nmain { let flag = true if flag { print 1 } else { print 2 } }\n",
    )
    .expect("document");
    let entry = |site: Site| CaptionEntry::site(site, caption("Send"));
    let other = parse_document("kernel 1\nnumbers float\nmain { }\n").expect("document");
    let refusals = [
        (
            vec![entry(main_site(&[9]))],
            CaptionsRefusal::Site {
                site: main_site(&[9]),
            },
        ),
        (
            vec![entry(Site::new(
                Unit::Function(Name::new("missing")),
                Vec::new(),
            ))],
            CaptionsRefusal::Site {
                site: Site::new(Unit::Function(Name::new("missing")), Vec::new()),
            },
        ),
        (
            vec![entry(main_site(&[]))],
            CaptionsRefusal::NotCaptionable {
                site: main_site(&[]),
            },
        ),
        // A then-arm is named by its statement; a condition is no step.
        (
            vec![entry(main_site(&[1, 1]))],
            CaptionsRefusal::NotCaptionable {
                site: main_site(&[1, 1]),
            },
        ),
        (
            vec![entry(main_site(&[1, 0]))],
            CaptionsRefusal::NotCaptionable {
                site: main_site(&[1, 0]),
            },
        ),
        (
            vec![entry(main_site(&[0])), entry(main_site(&[0]))],
            CaptionsRefusal::Duplicate {
                target: CaptionTarget::Site(main_site(&[0])),
            },
        ),
    ];
    for (entries, expected) in refusals {
        assert_eq!(Captions::new(&document, entries), Err(expected));
    }
    let valid = Captions::new(
        &document,
        vec![
            entry(main_site(&[1, 2])),
            entry(main_site(&[0])),
            entry(Site::new(Unit::Function(Name::new("helper")), Vec::new())),
            entry(main_site(&[1, 1, 0])),
        ],
    )
    .expect("statements, an else arm and a function root");
    assert_eq!(valid.validate(&other), Err(CaptionsRefusal::Document));
    assert_eq!(valid.at(&main_site(&[1, 2])), Some(&caption("Send")));
    assert_eq!(valid.at(&main_site(&[1])), None);
    // Decoding checks the order; validation then checks the document.
    let mut json = serde_json::to_value(&valid).expect("encodes");
    assert_eq!(
        serde_json::from_value::<Captions>(json.clone()).expect("decodes"),
        valid
    );
    json["entries"].as_array_mut().expect("entries").swap(0, 1);
    assert!(serde_json::from_value::<Captions>(json.clone()).is_err());
    let first = json["entries"][1].clone();
    json["entries"][0] = first;
    assert!(serde_json::from_value::<Captions>(json).is_err());
}

/// `K-CAP-007`: a caption names an entry's parameter by its name in the
/// signature, and a field by a path of record field names through its
/// parameter's or result's type, walking a list to its element and an
/// optional value to the value. Validation refuses a parameter the function
/// lacks and a path the types do not resolve. Rekeying keeps a parameter's
/// and a field's caption while its name resolves, never touching it, and
/// drops it once the parameter is gone.
fn signature_targets() {
    use lash_kernel_doc::{FieldPath, FieldRoot, FieldTarget, ParamTarget};

    let document = parse_document(concat!(
        "kernel 1\nnumbers float\n",
        "entry flow(event: Record{answer: Record{choice: Text}}, ",
        "items: List(Union(Record{name: Text, tags: Map(Text, Record{x: Int})}, Null)), n: Int) ",
        "-> Record{seen: Int}\n",
        "fn flow(event, items, n) { return null }\n",
        "fn helper(x) { return x }\n",
        "main { }\n",
    ))
    .expect("document");
    let param = |function: &str, param: &str| {
        CaptionTarget::Param(ParamTarget {
            function: Name::new(function),
            param: Name::new(param),
        })
    };
    let field = |function: &str, root: Option<&str>, fields: &[&str]| {
        CaptionTarget::Field(FieldTarget {
            function: Name::new(function),
            path: FieldPath {
                root: root.map_or(FieldRoot::Result, |root| FieldRoot::Param(Name::new(root))),
                fields: fields.iter().map(|field| (*field).to_owned()).collect(),
            },
        })
    };
    let entry = |target: CaptionTarget| CaptionEntry {
        target,
        caption: caption("Named"),
    };
    let refused = |target: CaptionTarget| match &target {
        CaptionTarget::Param(param) => CaptionsRefusal::Param {
            target: param.clone(),
        },
        CaptionTarget::Field(field) => CaptionsRefusal::Field {
            target: field.clone(),
        },
        CaptionTarget::Site(_) => unreachable!("a parameter or a field"),
    };
    let unresolved = [
        param("flow", "missing"),
        param("absent", "x"),
        field("flow", Some("event"), &["answer", "missing"]),
        // A text has no fields; a map, a record's rest and a bare name are
        // not walked.
        field("flow", Some("event"), &["answer", "choice", "length"]),
        field("flow", Some("items"), &["tags", "x"]),
        field("flow", Some("event"), &[]),
        field("flow", Some("missing"), &["answer"]),
        // A function that is no entry states no types.
        field("helper", Some("x"), &["y"]),
    ];
    for target in unresolved {
        assert_eq!(
            Captions::new(&document, vec![entry(target.clone())]),
            Err(refused(target.clone())),
            "{target}"
        );
    }
    let named = [
        param("flow", "n"),
        param("helper", "x"),
        field("flow", Some("event"), &["answer"]),
        field("flow", Some("event"), &["answer", "choice"]),
        // Through the list and the optional record.
        field("flow", Some("items"), &["name"]),
        field("flow", None, &["seen"]),
    ];
    let captions = Captions::new(&document, named.iter().cloned().map(entry).collect())
        .expect("every parameter and resolved path");
    let listed: BTreeSet<CaptionTarget> = CaptionKind::captionable(&document)
        .into_iter()
        .filter(|(_, kind)| matches!(kind, CaptionKind::Param | CaptionKind::Field))
        .map(|(target, _)| target)
        .collect();
    assert!(named.iter().all(|target| listed.contains(target)));
    assert_eq!(
        captions.get(&field("flow", None, &["seen"])),
        Some(&caption("Named"))
    );
    // The JSON form names each target by its own key.
    let json = serde_json::to_value(&captions).expect("encodes");
    assert_eq!(
        json["entries"][0]["param"],
        serde_json::json!({"function": "flow", "param": "n"})
    );
    assert_eq!(
        serde_json::from_value::<Captions>(json.clone()).expect("decodes"),
        captions
    );
    let mut two = json.clone();
    two["entries"][0]["site"] = serde_json::json!({"unit": "main"});
    assert!(serde_json::from_value::<Captions>(two).is_err());

    // Inserting a statement moves no parameter or field: their captions
    // stay, and none is touched.
    let base = document.identity();
    let _send = EffectName::new("send").expect("a tool's name");
    let registry = lash_kernel_doc::FunctionRegistry::new();
    let environment = lash_kernel_check::Environment::new(&registry);
    let mut draft = Draft::open(document.clone()).expect("a draft");
    let insert = Operation::InsertStep {
        base,
        at: Position::before(Site::new(Unit::Function(Name::new("flow")), [0]))
            .expect("a statement of the body"),
        slot: lash_vocabulary::ImportSlot(0),
        args: vec![DataValue::Literal(Literal::Text("new".into()))],
        result: Type::Any,
        binding: None,
    };
    let applied = draft
        .apply(
            &Transaction {
                base,
                edits: vec![insert],
            },
            &environment,
        )
        .expect("a step inserted in the body");
    let rekeyed = captions
        .rekey(&applied.correspondence, draft.document())
        .expect("the edit starts at the captions");
    assert_eq!(rekeyed.captions.entries(), captions.entries());
    assert!(rekeyed.touched.is_empty());

    // A parameter removed takes its caption and its fields' with it; the
    // others keep theirs.
    let removed = parse_document(concat!(
        "kernel 1\nnumbers float\n",
        "entry flow(items: List(Union(Record{name: Text, tags: Map(Text, Record{x: Int})}, Null)), n: Int) ",
        "-> Record{seen: Int}\n",
        "fn flow(items, n) { return null }\n",
        "fn helper(x) { return x }\n",
        "main { }\n",
    ))
    .expect("document");
    let roots = ["flow", "helper"]
        .map(|function| Site::new(Unit::Function(Name::new(function)), Vec::new()));
    let correspondence = lash_kernel_doc::Correspondence::of(
        base,
        removed.identity(),
        roots
            .iter()
            .map(|root| lash_kernel_doc::Survivor::new(root.clone(), root.clone(), false))
            .collect(),
    )
    .expect("a correspondence");
    let rekeyed = captions
        .rekey(&correspondence, &removed)
        .expect("the correspondence starts at the captions");
    let kept: Vec<CaptionTarget> = rekeyed
        .captions
        .entries()
        .iter()
        .map(|entry| entry.target.clone())
        .collect();
    assert_eq!(
        kept,
        [
            param("flow", "n"),
            param("helper", "x"),
            field("flow", Some("items"), &["name"]),
            field("flow", None, &["seen"]),
        ]
    );
    assert!(rekeyed.touched.is_empty());
}

/// `K-CAP-007`, for the captions a dialect imports: each authored entry's
/// view shows its parameters' captions by their position in its signature
/// and its fields' by path, and some case of a dialect that spells them
/// captions both.
fn signature_view(dialect: &CaptionDialect<'_>) -> Result<(), crate::HarnessError> {
    let fail =
        |case: &str, message: String| crate::HarnessError(format!("K-CAP-007 {case}: {message}"));
    let (mut params, mut fields) = (0, 0);
    for case in &dialect.corpus.authored {
        let lowered = dialect
            .package
            .front_end
            .lower(&case.source, &dialect.environment, Mode::Module)
            .map_err(|diagnostic| fail(&case.name, format!("{diagnostic:?}")))?;
        let document = lowered.document.document();
        for (name, export) in &document.exports {
            let lash_kernel_doc::Export::Function(entry) = export else {
                continue;
            };
            let (content, link) = crate::linked::content(document);
            let reference = lash_vocabulary::LinkedStart {
                link,
                selector: lash_vocabulary::StartSelector::Main,
            };
            let context =
                lash_kernel_view::ViewContext::new(reference.clone(), content, Default::default())
                    .map_err(|error| fail(&case.name, error.to_string()))?;
            let holder = lash_kernel_check::WorkflowDocument::entered(document.clone(), reference)
                .map_err(|error| fail(&case.name, error.to_string()))?;
            let view = lash_kernel_view::derive_callable(
                &holder,
                &Unit::Library(entry.function),
                &context,
            )
            .map_err(|error| fail(&case.name, error.to_string()))?;
            let joined = view
                .captions(&holder, &lowered.captions)
                .map_err(|error| fail(&case.name, error.to_string()))?;
            let expected_params: Vec<(usize, Name, Caption)> = entry
                .signature
                .params
                .iter()
                .enumerate()
                .filter_map(|(index, param)| {
                    let target = CaptionTarget::Param(lash_kernel_doc::ParamTarget {
                        function: name.clone(),
                        param: param.name.clone(),
                    });
                    Some((
                        index,
                        param.name.clone(),
                        lowered.captions.get(&target)?.clone(),
                    ))
                })
                .collect();
            let shown_params: Vec<(usize, Name, Caption)> = joined
                .params
                .iter()
                .map(|param| (param.index, param.name.clone(), param.caption.clone()))
                .collect();
            if shown_params != expected_params {
                return Err(fail(
                    &case.name,
                    format!("view params {shown_params:?}, expected {expected_params:?}"),
                ));
            }
            let expected_fields: Vec<_> = lowered
                .captions
                .entries()
                .iter()
                .filter_map(|caption| match &caption.target {
                    CaptionTarget::Field(field) if field.function == *name => {
                        Some((field.path.clone(), caption.caption.clone()))
                    }
                    _ => None,
                })
                .collect();
            let shown_fields: Vec<_> = joined
                .fields
                .iter()
                .map(|field| (field.path.clone(), field.caption.clone()))
                .collect();
            if shown_fields != expected_fields {
                return Err(fail(
                    &case.name,
                    format!("view fields {shown_fields:?}, expected {expected_fields:?}"),
                ));
            }
            params += shown_params.len();
            fields += shown_fields.len();
        }
    }
    let spells = |kind| dialect.package.captions.supports(kind);
    if (spells(CaptionKind::Param) && params == 0) || (spells(CaptionKind::Field) && fields == 0) {
        return Err(fail(
            "corpus",
            "no authored case captions a parameter and a field".into(),
        ));
    }
    Ok(())
}

/// Texts at the boundaries the rule admits.
fn admitted() -> Vec<(String, Option<String>)> {
    crate::captions::boundary_captions()
        .expect("boundary texts")
        .into_iter()
        .map(|caption| {
            (
                caption.title().to_owned(),
                caption.description().map(str::to_owned),
            )
        })
        .chain([(
            "t".repeat(200),
            Some(format!("{}\n\n{}", "a".repeat(1000), "b".repeat(1046))),
        )])
        .collect()
}

/// `K-CAP-004`: after one edit of each operation, rekeyed captions are
/// exactly the survivors at their successors' sites, and `touched` names
/// those beneath which the edit wrote.
fn rekey() {
    let document = parse_document(concat!(
        "kernel 1\nnumbers float\neffect send(value: Any) -> Any contract \"1\"\n",
        "entry helper(item: Record{title: Text}) -> Any\n",
        "fn helper(item) { return null }\n",
        "main {\n",
        " do perform send(\"a\") as Any\n",
        " let flag = true\n",
        " if flag { do perform send(\"b\") as Any } else { print 2 }\n",
        " try { do perform send(\"d\") as Any } catch e { print e } finally { print 1 }\n",
        " for item in [1] { print item } else { print 2 }\n",
        " if true { do perform send(\"u\") as Any }\n",
        " finish null\n",
        "}\n",
    ))
    .expect("document");
    let registry = lash_kernel_doc::FunctionRegistry::new();
    let environment = lash_kernel_check::Environment::new(&registry);
    let captionable = CaptionKind::captionable(&document);
    let kinds: BTreeSet<CaptionKind> = captionable.iter().map(|(_, kind)| *kind).collect();
    assert_eq!(kinds, CaptionKind::ALL.into_iter().collect(), "every kind");
    let captions = Captions::new(
        &document,
        captionable
            .iter()
            .enumerate()
            .map(|(index, (target, _))| CaptionEntry {
                target: target.clone(),
                caption: caption(&format!("Step {index}")),
            })
            .collect(),
    )
    .expect("every captionable site");
    let base = document.identity();
    let text = |value: &str| DataValue::Literal(Literal::Text(value.into()));
    let edits: [(&str, Operation, &[&[u32]]); 7] = [
        (
            "set a tool argument",
            Operation::SetValue {
                base,
                target: ValueTarget::ToolArgument {
                    action: main_site(&[0, 0]),
                    index: 0,
                    path: Vec::new(),
                },
                value: text("z"),
            },
            &[&[0]],
        ),
        (
            "change a condition",
            Operation::ChangeCondition {
                base,
                statement: main_site(&[2]),
                condition: Expr::Literal(Literal::Bool(false)),
            },
            &[&[2]],
        ),
        (
            "insert a step in an arm",
            Operation::InsertStep {
                base,
                at: Position::end(main_site(&[2, 1])),
                slot: lash_vocabulary::ImportSlot(0),
                args: vec![text("new")],
                result: Type::Any,
                binding: None,
            },
            &[&[2]],
        ),
        (
            "remove a step",
            Operation::RemoveStep {
                base,
                action: main_site(&[0, 0]),
            },
            &[],
        ),
        (
            "move a step",
            Operation::MoveStep {
                base,
                action: main_site(&[0, 0]),
                to: Position::end(main_site(&[4, 1])),
            },
            &[],
        ),
        (
            "wrap a step",
            Operation::WrapSteps {
                base,
                first: main_site(&[0, 0]),
                last: main_site(&[0, 0]),
                wrapper: Wrapper::If {
                    condition: Expr::Literal(Literal::Bool(true)),
                },
            },
            &[],
        ),
        (
            "unwrap a step",
            Operation::UnwrapSteps {
                base,
                wrapper: main_site(&[5]),
                branch: Branch::Then,
            },
            &[],
        ),
    ];
    for (name, operation, touched) in edits {
        let mut draft = Draft::open(document.clone()).expect("a draft");
        let applied = draft
            .apply(
                &Transaction {
                    base,
                    edits: vec![operation],
                },
                &environment,
            )
            .unwrap_or_else(|refusal| panic!("{name}: {refusal:?}"));
        let result = draft.document();
        let correspondence = &applied.correspondence;
        let rekeyed = captions
            .rekey(correspondence, result)
            .unwrap_or_else(|| panic!("{name}: the correspondence starts at the captions"));
        let expected: Vec<CaptionEntry> = {
            let mut kept: Vec<_> = captions
                .entries()
                .iter()
                .filter_map(|entry| {
                    let target = successor(correspondence, &entry.target)?;
                    target.kind(result)?;
                    Some(CaptionEntry {
                        target,
                        caption: entry.caption.clone(),
                    })
                })
                .collect();
            kept.sort_by(|a, b| a.target.cmp(&b.target));
            kept
        };
        assert_eq!(rekeyed.captions.entries(), expected.as_slice(), "{name}");
        assert_eq!(rekeyed.captions.document(), result.identity(), "{name}");
        rekeyed.captions.validate(result).expect(name);
        let touched: Vec<Site> = touched.iter().map(|path| main_site(path)).collect();
        assert_eq!(rekeyed.touched, touched, "{name}");
        match name {
            // The removed step's caption goes with it.
            "remove a step" => assert!(
                expected
                    .iter()
                    .all(|entry| Some(&entry.caption) != captions.at(&main_site(&[0])))
            ),
            // A moved or wrapped step keeps its caption where it went.
            "move a step" => assert_eq!(
                rekeyed.captions.at(&main_site(&[3, 1, 1])),
                captions.at(&main_site(&[0]))
            ),
            "wrap a step" => assert_eq!(
                rekeyed.captions.at(&main_site(&[0, 1, 0])),
                captions.at(&main_site(&[0]))
            ),
            // The unwrapped statement's caption moves out; its wrapper's and
            // the wrapper's empty else arm's go.
            "unwrap a step" => {
                assert_eq!(
                    rekeyed.captions.at(&main_site(&[5])),
                    captions.at(&main_site(&[5, 1, 0]))
                );
                assert_eq!(
                    rekeyed.captions.entries().len(),
                    captions.entries().len() - 2
                );
            }
            _ => assert_eq!(rekeyed.captions.entries().len(), captions.entries().len()),
        }
    }
    let other = parse_document("kernel 1\nnumbers float\nmain { }\n").expect("document");
    let foreign = Captions::empty(other.identity());
    let identity = lash_kernel_doc::Correspondence::identity(base, Vec::new())
        .expect("an empty correspondence");
    assert!(foreign.rekey(&identity, &document).is_none());
    assert!(captions.rekey(&identity, &other).is_none());
}

/// `K-CAP-004`, for the captions a dialect imports: a step inserted ahead of
/// each authored case's entry body keeps every caption at its step's new
/// site, and touches only the entry's own, beneath which the step is new.
fn rekey_imported(dialect: &CaptionDialect<'_>) -> Result<(), crate::HarnessError> {
    for case in &dialect.corpus.authored {
        let lowered = dialect
            .package
            .front_end
            .lower(&case.source, &dialect.environment, Mode::Module)
            .map_err(|diagnostic| {
                crate::HarnessError(format!("K-CAP-004 {}: {diagnostic:?}", case.name))
            })?;
        let document = lowered.document.document();
        let entry = document
            .exports
            .values()
            .find_map(|export| match export {
                lash_kernel_doc::Export::Function(entry) => Some(entry),
                _ => None,
            })
            .expect("a module has a function export");
        let root = Site::new(Unit::Library(entry.function), Vec::new());
        let (effect, signature) = document
            .imports
            .iter()
            .find_map(|(slot, import)| match &import.kind {
                lash_vocabulary::ImportKind::Operation { signature } => Some((slot, signature)),
                _ => None,
            })
            .expect("the corpus tool");
        let environment = lash_kernel_check::Environment {
            functions: dialect.environment.library,
            bindings: BTreeSet::new(),
        };
        let base = lowered.document.id();
        let insert = Operation::InsertStep {
            base,
            at: Position::before(root.child(0)).expect("a statement of the body"),
            slot: *effect,
            args: signature
                .params
                .iter()
                .map(|_| DataValue::Literal(Literal::Text("new".into())))
                .collect(),
            result: Type::Any,
            binding: None,
        };
        let mut draft = Draft::open(document.clone()).expect("a draft");
        let applied = draft
            .apply(
                &Transaction {
                    base,
                    edits: vec![insert],
                },
                &environment,
            )
            .unwrap_or_else(|refusal| panic!("{}: {refusal:?}", case.name));
        let result = draft.document();
        let rekeyed = lowered
            .captions
            .rekey(&applied.correspondence, result)
            .expect("the correspondence starts at the captions");
        let mut expected: Vec<CaptionEntry> = lowered
            .captions
            .entries()
            .iter()
            .map(|entry| CaptionEntry {
                target: successor(&applied.correspondence, &entry.target)
                    .expect("an insertion removes nothing"),
                caption: entry.caption.clone(),
            })
            .collect();
        expected.sort_by(|a, b| a.target.cmp(&b.target));
        // Every caption in the body moved one statement on.
        assert_ne!(
            expected.as_slice(),
            lowered.captions.entries(),
            "{}",
            case.name
        );
        assert_eq!(
            rekeyed.captions.entries(),
            expected.as_slice(),
            "{}",
            case.name
        );
        let touched: Vec<Site> = lowered
            .captions
            .at(&root)
            .map(|_| root)
            .into_iter()
            .collect();
        assert_eq!(rekeyed.touched, touched, "{}", case.name);
    }
    Ok(())
}

/// `K-CAP-004`, of the captions a dialect's source spells: a canvas edit of
/// a tool's argument, and one of a condition, keep every imported caption
/// at its site and touch exactly those at or above what they write.
fn rekey_canvas_edits(dialect: &CaptionDialect<'_>) -> Result<(), crate::HarnessError> {
    let fail =
        |case: &str, message: String| crate::HarnessError(format!("K-CAP-004 {case}: {message}"));
    let mut edits = 0;
    for case in &dialect.corpus.authored {
        let lowered = dialect
            .package
            .front_end
            .lower(&case.source, &dialect.environment, Mode::Module)
            .map_err(|diagnostic| fail(&case.name, format!("{diagnostic:?}")))?;
        let document = lowered.document.document();
        let environment = lash_kernel_check::Environment {
            functions: dialect.environment.library,
            bindings: BTreeSet::new(),
        };
        let base = document.identity();
        let mut operations = Vec::new();
        // An argument that runs an action or holds a closure is no canvas
        // edit's to set.
        if let Some(action) = first_site(document, |node| match node {
            Node::Expr(Expr::Act(act)) => {
                matches!(act.kind, ActKind::Perform { .. })
                    && act.args.first().is_some_and(|argument| {
                        !acts(Node::Expr(argument)) && !closes(Node::Expr(argument))
                    })
            }
            Node::Action(Action::Perform { args, .. }) => !args.is_empty(),
            _ => false,
        }) {
            operations.push((
                action.clone(),
                Operation::SetValue {
                    base,
                    target: ValueTarget::ToolArgument {
                        action,
                        index: 0,
                        path: Vec::new(),
                    },
                    value: DataValue::Literal(Literal::Text("edited".into())),
                },
            ));
        }
        // A condition that runs an action is no canvas edit's to change.
        if let Some(statement) = first_site(
            document,
            |node| matches!(node, Node::Stmt(Stmt::If { condition, .. }) if !acts(Node::Expr(condition))),
        ) {
            operations.push((
                statement.clone(),
                Operation::ChangeCondition {
                    base,
                    statement,
                    condition: Expr::Literal(Literal::Bool(false)),
                },
            ));
        }
        for (written, operation) in operations {
            let mut draft = Draft::open(document.clone())
                .map_err(|refusal| fail(&case.name, format!("{refusal:?}")))?;
            let applied = draft
                .apply(
                    &Transaction {
                        base,
                        edits: vec![operation],
                    },
                    &environment,
                )
                .map_err(|refusal| fail(&case.name, format!("{refusal:?}")))?;
            let rekeyed = lowered
                .captions
                .rekey(&applied.correspondence, draft.document())
                .ok_or_else(|| fail(&case.name, "the edit starts elsewhere".into()))?;
            if rekeyed.captions.entries() != lowered.captions.entries() {
                return Err(fail(
                    &case.name,
                    format!("rekeyed {:?}", rekeyed.captions.entries()),
                ));
            }
            let above: Vec<Site> = lowered
                .captions
                .entries()
                .iter()
                .filter_map(|entry| entry.target.site().cloned())
                .filter(|site| site.unit == written.unit && written.path.starts_with(&site.path))
                .collect();
            if rekeyed.touched != above {
                return Err(fail(
                    &case.name,
                    format!("touched {:?}, expected {above:?}", rekeyed.touched),
                ));
            }
            edits += 1;
        }
    }
    if edits == 0 && !dialect.package.captions.kinds.is_empty() {
        return Err(fail("corpus", "no authored case takes an edit".into()));
    }
    Ok(())
}

/// Where `target` goes through `correspondence`: a site to its node's
/// successor, a parameter or a field to the same name of its function's
/// body's successor.
fn successor(
    correspondence: &lash_kernel_doc::Correspondence,
    target: &CaptionTarget,
) -> Option<CaptionTarget> {
    Some(match target {
        CaptionTarget::Site(site) => CaptionTarget::Site(correspondence.successor(site)?.clone()),
        CaptionTarget::Param(param) => {
            let Unit::Function(function) = &correspondence
                .successor(&Site::new(
                    Unit::Function(param.function.clone()),
                    Vec::new(),
                ))?
                .unit
            else {
                return None;
            };
            CaptionTarget::Param(lash_kernel_doc::ParamTarget {
                function: function.clone(),
                param: param.param.clone(),
            })
        }
        CaptionTarget::Field(field) => {
            let Unit::Function(function) = &correspondence
                .successor(&Site::new(
                    Unit::Function(field.function.clone()),
                    Vec::new(),
                ))?
                .unit
            else {
                return None;
            };
            CaptionTarget::Field(lash_kernel_doc::FieldTarget {
                function: function.clone(),
                path: field.path.clone(),
            })
        }
    })
}

/// The Site of the first node of `document` that `wanted` picks, in a walk
/// of its declared functions and then of `main`.
fn first_site(document: &Document, wanted: impl Fn(Node<'_>) -> bool) -> Option<Site> {
    fn walk(node: Node<'_>, site: Site, wanted: &dyn Fn(Node<'_>) -> bool) -> Option<Site> {
        if wanted(node) {
            return Some(site);
        }
        (0u32..)
            .zip(node.children())
            .find_map(|(index, child)| walk(child, site.child(index), wanted))
    }
    document
        .units()
        .into_iter()
        .find_map(|(unit, body)| walk(Node::Block(body), Site::new(unit, Vec::new()), &wanted))
}

fn acts(node: Node<'_>) -> bool {
    matches!(node, Node::Action(_) | Node::Expr(Expr::Act(_)))
        || node.children().into_iter().any(acts)
}

fn closes(node: Node<'_>) -> bool {
    matches!(node, Node::Expr(Expr::Closure(_))) || node.children().into_iter().any(closes)
}
