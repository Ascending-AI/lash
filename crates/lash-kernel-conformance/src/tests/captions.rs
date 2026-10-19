//! The `K-CAP` laws: each rule's shard runs the generic caption harness for
//! every dialect, and the rules no dialect takes part in (rekeying and the
//! text domain) once, over kernel documents.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_dialect::{EffectControl, Environment, NamedLibrary, Package, SourceFormat};
use lash_kernel_doc::{
    Caption, CaptionEntry, CaptionField, CaptionKind, CaptionRefusal, Captions, CaptionsRefusal,
    EffectName, Expr, Literal, Name, Param, Signature, Site, Type, Unit, parse_document,
};
use lash_kernel_edit::authoring::{Branch, DataValue, Operation, ValueTarget, Wrapper};
use lash_kernel_edit::{Draft, Position, Transaction};

use crate::captions::{CaptionCorpus, CaptionDialect};

const TYPESCRIPT: &str = include_str!("../../captions/typescript.json");
const PYTHON: &str = include_str!("../../captions/python.json");

/// The rule's laws, for every dialect where a dialect takes part.
pub(super) fn verify(rule: &str) {
    match rule {
        "K-CAP-004" => rekey(),
        "K-CAP-005" => {
            text_domain();
            each_dialect(|dialect| dialect.printer_validates());
        }
        "K-CAP-001" => each_dialect(|dialect| dialect.behaviour_free()),
        "K-CAP-002" => each_dialect(|dialect| dialect.readback()),
        "K-CAP-003" => each_dialect(|dialect| dialect.authored()),
        "K-CAP-006" => each_dialect(|dialect| dialect.dropped()),
        _ => unreachable!("{rule} is not a caption rule"),
    }
}

fn each_dialect(law: impl Fn(&CaptionDialect<'_>) -> Result<(), crate::HarnessError>) {
    let effects = effects();
    let tool_roots = BTreeSet::from([Name::new("corpus")]);
    let controls: BTreeMap<EffectName, BTreeSet<EffectControl>> = BTreeMap::new();
    let dialects: [(Package, NamedLibrary, SourceFormat, &str); 2] = [
        (
            typescript(),
            library(lash_dialect_typescript::define_helpers),
            SourceFormat::TypeScriptV1,
            TYPESCRIPT,
        ),
        (
            lash_dialect_python::package(Vec::new()),
            library(lash_dialect_python::define_helpers),
            SourceFormat::PythonV1,
            PYTHON,
        ),
    ];
    for (package, library, format, corpus) in &dialects {
        let corpus = CaptionCorpus::from_json(corpus).expect("caption corpus");
        let dialect = CaptionDialect {
            package,
            environment: Environment {
                library,
                effects: &effects,
                tool_roots: &tool_roots,
                controls: &controls,
            },
            format: *format,
            corpus: &corpus,
        };
        law(&dialect).unwrap_or_else(|error| panic!("{}: {error}", package.dialect));
    }
}

fn typescript() -> Package {
    Package {
        dialect: "typescript".into(),
        front_end: Box::new(lash_dialect_typescript::TypeScript::default()),
        read: |source, base, limits| {
            lash_dialect_typescript::read(source, base, limits).map_err(Into::into)
        },
        printer: Some(Box::new(lash_dialect_typescript::TypeScript::default())),
        captions: lash_dialect_typescript::CAPTIONS,
        function_values: Some(lash_dialect_typescript::function_values()),
        functions: Vec::new(),
    }
}

fn library(
    define: fn(
        &mut NamedLibrary,
    ) -> Result<
        Vec<lash_kernel_doc::FunctionDefinition>,
        lash_kernel_dialect::SourceError,
    >,
) -> NamedLibrary {
    let mut registry = super::corpus_registry();
    lash_ext_date_ecma::register(&mut registry).expect("date extension");
    lash_ext_url_whatwg::register(&mut registry).expect("URL extension");
    let mut library = NamedLibrary::from_registry(&registry).expect("unique library names");
    define(&mut library).expect("dialect helpers");
    library
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
    let entry = |site: Site| CaptionEntry {
        site,
        caption: caption("Send"),
    };
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
                site: main_site(&[0]),
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
    assert_eq!(valid.get(&main_site(&[1, 2])), Some(&caption("Send")));
    assert_eq!(valid.get(&main_site(&[1])), None);
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
    let mut document = parse_document(concat!(
        "kernel 1\nnumbers float\neffect send(value: Any) -> Any\n",
        "fn helper() { return null }\n",
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
    let required = lash_kernel_check::requirements(&document, &registry);
    document
        .manifest
        .effects
        .retain(|effect, _| required.effects.contains(effect));
    let mut environment = lash_kernel_check::Environment::new(&registry);
    environment.effects.insert(
        EffectName::new("send").expect("a tool's name"),
        Signature {
            params: vec![Param {
                name: Name::new("value"),
                ty: Type::Any,
                optional: false,
            }],
            result: Type::Any,
        },
    );
    let captionable = CaptionKind::captionable(&document);
    let kinds: BTreeSet<CaptionKind> = captionable.iter().map(|(_, kind)| *kind).collect();
    assert_eq!(kinds, CaptionKind::ALL.into_iter().collect(), "every kind");
    let captions = Captions::new(
        &document,
        captionable
            .iter()
            .enumerate()
            .map(|(index, (site, _))| CaptionEntry {
                site: site.clone(),
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
                effect: EffectName::new("send").expect("a tool's name"),
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
                    let site = correspondence.successor(&entry.site)?;
                    CaptionKind::of(result, site)?;
                    Some(CaptionEntry {
                        site: site.clone(),
                        caption: entry.caption.clone(),
                    })
                })
                .collect();
            kept.sort_by(|a, b| a.site.cmp(&b.site));
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
                    .all(|entry| Some(&entry.caption) != captions.get(&main_site(&[0])))
            ),
            // A moved or wrapped step keeps its caption where it went.
            "move a step" => assert_eq!(
                rekeyed.captions.get(&main_site(&[3, 1, 1])),
                captions.get(&main_site(&[0]))
            ),
            "wrap a step" => assert_eq!(
                rekeyed.captions.get(&main_site(&[0, 1, 0])),
                captions.get(&main_site(&[0]))
            ),
            // The unwrapped statement's caption moves out; its wrapper's and
            // the wrapper's empty else arm's go.
            "unwrap a step" => {
                assert_eq!(
                    rekeyed.captions.get(&main_site(&[5])),
                    captions.get(&main_site(&[5, 1, 0]))
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
