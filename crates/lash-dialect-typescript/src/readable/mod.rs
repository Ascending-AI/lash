//! Readable export: a document printed as the TypeScript a model would
//! write, verified region by region.
//!
//! Every printed module must lower to the complete original document and
//! captions. If authored source cannot express it, printing refuses.

mod closures;
mod expressions;
mod labels;
mod lift;
mod methods;
mod mixed;
mod optional;
mod patterns;
mod primitives;
mod source;
mod statements;

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_dialect::{
    Environment, GeneratedSourceLimits, Library, PrintError, PrintOptions, PrintedSource, Printer,
    SourceFormat, check_document_depth, regions,
};
use lash_kernel_doc::{self as k};

use lash_kernel_value::Name;

use crate::Diagnostic;

/// Print authored TypeScript after checking its full lowering identity.
///
/// # Errors
/// Refuses a document that has no faithful authored representation or exceeds
/// the configured source limits.
pub fn print(
    document: &k::Document,
    captions: Option<&k::Captions>,
    library: &dyn Library,
    options: PrintOptions,
) -> Result<PrintedSource, PrintError> {
    let mut printed = print_attempt(document, captions, library, options, STYLES[0]);
    // Exact operation spellings come last: only a document whose convenient
    // reads and method calls do not read back is printed with them.
    for style in [false, true]
        .into_iter()
        .flat_map(|exact| {
            [false, true].into_iter().flat_map(move |block_bodies| {
                [false, true].into_iter().flat_map(move |const_functions| {
                    STYLES.iter().map(move |style| lift::Style {
                        block_bodies,
                        const_functions,
                        exact,
                        ..*style
                    })
                })
            })
        })
        .skip(1)
    {
        if !matches!(printed, Err(PrintError::Unprintable { .. })) {
            break;
        }
        printed = print_attempt(document, captions, library, options, style);
    }
    printed
}

/// Plainest first: operators over constructor calls, then temporaries
/// written where they are read over their own `const`s.
const STYLES: [lift::Style; 4] = [
    lift::Style {
        block_bodies: false,
        const_functions: false,
        operators: true,
        preserve_temporaries: false,
        exact: false,
    },
    lift::Style {
        block_bodies: false,
        const_functions: false,
        operators: true,
        preserve_temporaries: true,
        exact: false,
    },
    lift::Style {
        block_bodies: false,
        const_functions: false,
        operators: false,
        preserve_temporaries: false,
        exact: false,
    },
    lift::Style {
        block_bodies: false,
        const_functions: false,
        operators: false,
        preserve_temporaries: true,
        exact: false,
    },
];

fn print_attempt(
    document: &k::Document,
    captions: Option<&k::Captions>,
    library: &dyn Library,
    options: PrintOptions,
    style: lift::Style,
) -> Result<PrintedSource, PrintError> {
    check_document_depth(document, options.limits)?;
    crate::constructs::conventions::CONVENTIONS.check_document(document, library)?;
    let namespaces = regions::namespaces(document).map_err(PrintError::Namespace)?;
    let lifted = lift::document(document, captions, library, style);
    let printing = Printing {
        document,
        options,
        environment: Environment::new(library, &namespaces),
        items: lifted
            .functions
            .into_iter()
            .map(|(name, (function, item))| {
                item.map(|item| (name, item))
                    .map_err(|detail| PrintError::Unprintable {
                        reason: lash_kernel_dialect::UnprintableReason::Callable { detail },
                        site: lash_kernel_value::sites::Site::new(
                            lash_kernel_value::sites::Unit::Library(function),
                            Vec::new(),
                        ),
                    })
            })
            .collect::<Result<_, _>>()?,
        main: Some(lifted.main.map_err(|detail| PrintError::Unprintable {
            reason: lash_kernel_dialect::UnprintableReason::Main { detail },
            site: lash_kernel_value::sites::Site::new(
                lash_kernel_value::sites::Unit::Main,
                Vec::new(),
            ),
        })?),
    };
    let verified = printing.items.keys().cloned().collect();
    regions::print_verified(document, captions, options, verified, &printing, library)
}

/// One document's lifts, printed and read for [`regions::print_verified`].
struct Printing<'a> {
    document: &'a k::Document,
    options: PrintOptions,
    environment: Environment<'a>,
    items: BTreeMap<Name, source::Item>,
    main: Option<lift::Main>,
}

impl regions::Regions for Printing<'_> {
    fn format(&self) -> SourceFormat {
        SourceFormat::TypeScriptV1
    }

    fn whole(&self, functions: &BTreeSet<Name>) -> Result<regions::Whole, PrintError> {
        let mut writer = source::Writer::default();
        let mut written = BTreeMap::new();
        let unprintable = || PrintError::Unprintable {
            reason: lash_kernel_dialect::UnprintableReason::ModuleDeclarations,
            site: lash_kernel_value::sites::Site::new(
                lash_kernel_value::sites::Unit::Main,
                Vec::new(),
            ),
        };
        match self.main.as_ref().ok_or_else(unprintable)? {
            lift::Main::Module(parts) => {
                for (index, part) in parts.iter().enumerate() {
                    if index > 0 {
                        writer.out.push('\n');
                    }
                    match part {
                        lift::ModulePart::Export(name) => {
                            let item = self
                                .items
                                .get(name)
                                .filter(|_| functions.contains(name))
                                .ok_or_else(unprintable)?;
                            let start = writer.out.len();
                            writer.item(item);
                            written.insert(name.clone(), start..writer.out.len());
                        }
                        lift::ModulePart::Alias { local, exported } => {
                            writer.export_alias(local.as_str(), exported.as_str())
                        }
                        lift::ModulePart::Statements(statements) => writer.top_level(statements),
                    }
                }
            }
            lift::Main::Cell(statements) => writer.top_level(statements),
        }
        Ok(regions::Whole {
            source: writer.out,
            functions: written,
        })
    }

    fn read(&self, source: &str) -> Result<regions::RegionRead, Option<usize>> {
        mixed::read(
            source,
            self.document,
            &self.environment,
            self.options.limits,
            None,
        )
        .map_err(|diagnostic| diagnostic.span().map(|span| span.start))
    }
}

/// Read ordinary TypeScript using the base document's declared signatures.
///
/// # Errors
/// The front end's diagnostic for invalid or unsupported authored source.
pub fn read(
    source: &str,
    base: &k::Document,
    library: &dyn Library,
    limits: GeneratedSourceLimits,
    source_limits: Option<lash_kernel_dialect::workflow::WorkflowSourceLimits>,
) -> Result<(k::Document, k::Captions), Diagnostic> {
    let namespaces = regions::namespaces(base).map_err(|refused| {
        Diagnostic::new(
            crate::DiagnosticCode::InvalidAst,
            format!("the base document's operations compose no namespace: {refused}"),
            None,
        )
    })?;
    let environment = Environment::new(library, &namespaces);
    let source_limits = match source_limits {
        Some(limits) => crate::SourceLimits::new(limits.max_bytes, limits.max_nesting)?
            .with_implicit_text_conversion(limits.implicit_text_conversion),
        None => crate::SourceLimits::new(source.len().max(1), crate::MAX_SOURCE_NESTING_DEPTH)?,
    };
    let read = mixed::read(source, base, &environment, limits, Some(source_limits))?;
    Ok((read.document, read.captions))
}

/// The TypeScript printer, as a dialect package holds it.
impl Printer for crate::TypeScript {
    fn print(
        &self,
        document: &k::Document,
        captions: Option<&k::Captions>,
        library: &dyn Library,
        options: PrintOptions,
    ) -> Result<PrintedSource, PrintError> {
        print(document, captions, library, options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIG-7664, FIG-7698: every helper and construct the lowering emits
    /// in a function body, for every operator, built-in table row and
    /// statement form, needs an exact authored spelling, including native
    /// constructors such as RegExp.
    #[test]
    fn syntax_lowered_helpers_have_source_spellings() {
        let library = crate::tests::library();
        let namespaces = Default::default();
        let environment = Environment::new(library, &namespaces);
        let mut forms = crate::lower::inventory::forms();
        let admitted = forms.len();
        forms.extend(crate::lower::inventory::members());
        let mut inventory = BTreeMap::new();
        let mut failures = Vec::new();
        for (index, (name, source)) in forms.iter().enumerate() {
            let (lowered, emitted) = crate::lower::inventory::collect(|| {
                crate::lower(source, &environment, lash_kernel_dialect::Mode::Module)
            });
            let lowered = match lowered {
                Ok(lowered) => lowered,
                Err(_) if index >= admitted => continue,
                Err(error) => panic!("{name}: {error}"),
            };
            for (helper, identity) in emitted {
                inventory.entry(helper).or_insert((identity, index));
            }
            match print(&lowered.document, None, library, Default::default()) {
                Ok(printed) => {
                    let read = crate::lower(
                        &printed.source,
                        &environment,
                        lash_kernel_dialect::Mode::Module,
                    )
                    .unwrap();
                    assert_eq!(
                        read.document.canonical_bytes(),
                        lowered.document.canonical_bytes(),
                        "{name}: {}",
                        printed.source
                    );
                }
                Err(error) => failures.push(format!("{name}: {error}")),
            }
        }
        for (helper, (identity, index)) in &inventory {
            let (name, source) = &forms[*index];
            let lowered =
                crate::lower(source, &environment, lash_kernel_dialect::Mode::Module).unwrap();
            if let Ok(printed) = print(&lowered.document, None, library, Default::default()) {
                let (_, emitted) = crate::lower::inventory::collect(|| {
                    crate::lower(
                        &printed.source,
                        &environment,
                        lash_kernel_dialect::Mode::Module,
                    )
                    .unwrap()
                });
                assert_eq!(
                    emitted.get(helper),
                    Some(identity),
                    "{helper}, lowered by {name}, must retain its exact spelling"
                );
            }
        }
        println!("{} emitted syntax helpers checked", inventory.len());
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// FIG-7706: a local's declared type (`let cursor: string | null`) is
    /// source evidence beside the callable, keyed by its binding: a shadowing
    /// `cursor` of another type keeps its own. The printed callable reads back
    /// exactly, with no forwarding call the annotation's absence would add,
    /// as captured and after its value is stored and carried; without the
    /// evidence it is unprintable.
    #[test]
    fn declared_local_types_print_from_their_evidence() {
        let source = r#"try {
const created=await workflows.create({space_id:'01a1b3dd-b832-74e9-b62c-180a67f6a34f',name:'signup-counts-report',description:'Posts the current three weekly signup counts to the Signup counts room. No schedule is configured.',run:async ():Promise<unknown>=>{
 let cursor:string|null=null;
 { const cursor:number=1; if(cursor<0) throw new Error('never'); }
 let text='';
 let citation='';
 do {
  const result=await cenote.readDocument({document:{doc_id:'01a1b3dd-b9cd-73d4-892f-2d0d287a2fe6'},cursor,limit:100});
  if(!result.ok) throw new Error('Page read failed');
  if(!citation && result.data.citation) citation=result.data.citation.uri;
  for(const item of result.data.items) { if('text' in item) text+=item.text+'\n'; }
  cursor=result.data.next_cursor;
 } while(cursor);
 const matches=text.match(/Week\s*[123]\s*:\s*\d+/gi);
 if(!matches || matches.length!==3 || !citation) throw new Error('Expected three weekly signup counts and a citation');
 for(const n of ['1','2','3']) { const found=matches.filter((s:string):boolean=>new RegExp('Week\\s*'+n+'\\s*:','i').test(s)); if(found.length!==1) throw new Error('Expected exactly one count per week'); }
 const body='Signup counts: '+matches.join('; ')+'. [Signup counts]('+citation+')';
 await cenote.discuss({action:'start',subject:{type:'space',space_id:'01a1b3dd-b832-74e9-b62c-180a67f6a34f'},visibility:'everyone',body});
 return {posted:true};
}});
console.log(JSON.stringify(created));
} catch(e) { console.log(JSON.stringify(e)); }"#;
        let library = crate::tests::library();
        let (_, namespaces) = corpus_namespaces();
        let environment = Environment::new(library, &namespaces);
        let lowered = crate::lower(
            source,
            &environment,
            lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment::first()),
        )
        .unwrap();
        // The printer proves structural readback; the printed module lowers
        // and prints again to the same source.
        let reads_back = |opened: &lash_kernel_package::MaterializedWorkflow| {
            let printed = print(
                &opened.document,
                Some(&opened.captions),
                library,
                Default::default(),
            )?;
            let read = crate::lower(
                &printed.source,
                &environment,
                lash_kernel_dialect::Mode::Module,
            )
            .unwrap();
            let reprinted = print(
                &read.document,
                Some(&read.captions),
                library,
                Default::default(),
            )
            .unwrap();
            assert_eq!(reprinted.source, printed.source);
            Ok::<_, PrintError>(())
        };
        reads_back(&materialize_callable(&lowered)).unwrap();
        let carried = materialize_callable_as(&lowered, |function| {
            let stored: k::FunctionValue = lash_kernel_value::tokens::decode_body(
                &lash_kernel_value::tokens::encode_body(&function),
            )
            .unwrap();
            assert_eq!(stored, function);
            k::FunctionValue::from_transport_json(
                &stored.transport_json().unwrap(),
                &k::FunctionRegistry::default(),
                Default::default(),
                lash_kernel_value::data_limits::KernelDataLimits::standard(),
            )
            .unwrap()
        });
        reads_back(&carried).unwrap();
        let bare = materialize_callable_as(&lowered, |mut function| {
            assert!(!function.declared_locals.is_empty());
            function.declared_locals.clear();
            function
        });
        let refused = reads_back(&bare);
        assert!(
            matches!(refused, Err(PrintError::Unprintable { .. })),
            "{refused:?}"
        );
    }

    fn materialize_callable(
        lowered: &lash_kernel_dialect::Lowered,
    ) -> lash_kernel_package::MaterializedWorkflow {
        materialize_callable_as(lowered, |function| function)
    }

    /// Materializes the callable of `lowered` as `saved` leaves its value.
    fn materialize_callable_as(
        lowered: &lash_kernel_dialect::Lowered,
        saved: impl FnOnce(k::FunctionValue) -> k::FunctionValue,
    ) -> lash_kernel_package::MaterializedWorkflow {
        let (site, written, closure) = lowered
            .written
            .iter()
            .find_map(|(site, written)| {
                let k::Node::Expr(k::Expr::Closure(closure)) = lowered.document.node(site)? else {
                    return None;
                };
                closure
                    .presentation
                    .as_ref()?
                    .signature
                    .params
                    .is_empty()
                    .then_some((site, written, closure))
            })
            .expect("zero-argument callable");
        let presentation = closure.presentation.as_ref().unwrap();
        let link = lash_kernel_value::Link {
            helper_fills: Default::default(),
            document: lowered.document.identity(),
            fills: Default::default(),
        };
        let function = k::FunctionValue {
            callable: lash_kernel_value::CallableId {
                link: link.identity(),
                function: closure.function,
            },
            captures: Default::default(),
            signature: presentation.signature.clone(),
            convention: presentation.convention.clone(),
            representation: presentation.representation.clone(),
            token: Some(vec![
                k::Datum::Text("ts.function".into()),
                k::Datum::Text(String::new()),
                k::Datum::Text(
                    written
                        .signature_origin
                        .as_ref()
                        .and_then(|origin| origin.name.clone())
                        .unwrap_or_default(),
                ),
                k::Datum::Float(k::Float::new(0.0)),
                k::Datum::Null,
                k::Datum::Record(vec![]),
            ]),
            written_signature: Some(k::WrittenSignature {
                language: written.language,
                signature: written.signature.clone(),
            }),
            signature_origin: written.signature_origin.clone(),
            implicit_text_conversions: written.implicit_text_conversions.clone(),
            declared_locals: written.declared_locals.clone(),
            captions: lowered.captions.clone(),
            creation: Some(site.clone()),
        };
        let function = saved(function);
        let content = lash_kernel_package::LinkedContent {
            documents: [(
                lowered.document.identity(),
                std::sync::Arc::clone(lowered.document.shared()),
            )]
            .into(),
            links: [(link.identity(), link)].into(),
            ..Default::default()
        };
        lash_kernel_package::materialize_workflow(
            &content,
            &k::WorkflowReference::Callable(function.callable),
            Some(&function),
            None,
            &lash_kernel_releases::helper_release().unwrap().helpers,
        )
        .unwrap()
    }

    /// The packages the authored corpus calls, with and without `processes`.
    fn corpus_namespaces() -> (
        lash_kernel_dialect::Namespaces,
        lash_kernel_dialect::Namespaces,
    ) {
        let mut packages = BTreeMap::new();
        for (package, member) in [
            ("people", "lookup"),
            ("information", "search"),
            ("information", "read"),
            ("io", "read_csv"),
            ("inbox", "send_item"),
            ("workflows", "create"),
            ("integrations", "execute"),
            ("views", "table"),
            ("cenote", "read_document"),
            ("cenote", "discuss"),
            ("views", "bar_chart"),
            ("views", "action"),
            ("views", "card"),
            ("corpus", "echo"),
            ("processes", "await"),
        ] {
            packages
                .entry(lash_kernel_value::PackagePath::parse(package).unwrap())
                .or_insert_with(BTreeMap::new)
                .insert(
                    lash_kernel_value::Segment::new(member).unwrap(),
                    lash_kernel_dialect::NamespaceMember {
                        visibility: lash_kernel_value::Visibility::Public,
                        kind: lash_kernel_dialect::NamespaceKind::Operation(
                            lash_kernel_dialect::OperationSignature {
                                signature: lash_kernel_value::Signature {
                                    params: (0..match (package, member) {
                                        ("views", "bar_chart") => 4,
                                        ("views", "action") => 3,
                                        ("views", "card" | "table") => 2,
                                        _ => 1,
                                    })
                                        .map(|index| lash_kernel_value::types::Param {
                                            name: Name::new(format!("input{index}")),
                                            ty: lash_kernel_value::types::Type::Any,
                                            optional: false,
                                        })
                                        .collect(),
                                    result: if package == "io" {
                                        lash_kernel_value::types::Type::Text
                                    } else {
                                        lash_kernel_value::types::Type::Any
                                    },
                                },
                                controls: lash_kernel_value::TurnControls::none(),
                                mode: lash_kernel_value::OperationMode::Waited,
                            },
                        ),
                    },
                );
        }
        let process_namespaces =
            lash_kernel_dialect::Namespaces::new(packages.clone(), Default::default(), &|_| false)
                .unwrap();
        packages.remove(&lash_kernel_value::PackagePath::parse("processes").unwrap());
        let namespaces =
            lash_kernel_dialect::Namespaces::new(packages, Default::default(), &|_| false).unwrap();
        (process_namespaces, namespaces)
    }

    /// FIG-7474: helpers and const-bound arrows must print without losing
    /// authored function names or changing the complete lowered document.
    #[test]
    fn authored_corpus_prints_and_relowers_identically() {
        #[derive(serde::Deserialize)]
        struct Case {
            name: String,
            source: String,
            #[serde(default)]
            cell: bool,
            #[serde(default)]
            materialize: bool,
            #[serde(default)]
            processes: bool,
            #[serde(default)]
            names: Vec<String>,
        }
        let library = crate::tests::library();
        let (process_namespaces, namespaces) = corpus_namespaces();
        let mut cases: Vec<Case> = serde_json::from_str(include_str!("corpus.json")).unwrap();
        let mut expressions = vec![
            (
                "regex constructor receiver".to_owned(),
                r#"new RegExp("Week \\d+").test("Week 12")"#.to_owned(),
            ),
            (
                "object arrow callback".to_owned(),
                "[1, 2].map(value => ({value}))".to_owned(),
            ),
        ];
        for literal in [r"/Week \d+/g", r"/\//", r"/[a-z/]+/gi"] {
            for (position, context) in [
                ("receiver", r#"REGEX.test("Week 12")"#),
                ("argument", r#"("Week 12" as any).matchAll(REGEX)"#),
                ("spread", r#"[...("Week 12" as any).matchAll(REGEX)]"#),
                (
                    "bound array callback",
                    r#"(() => { const found = [...("Week 12" as any).matchAll(REGEX)]; return found.map(m => m[0]); })()"#,
                ),
                ("array", "[0, REGEX, 1]"),
                ("object value", "({pattern: REGEX})"),
                (
                    "binding default",
                    "(() => { const {pattern = REGEX} = {}; return pattern; })()",
                ),
                ("parameter default", "((pattern = REGEX) => pattern)()"),
                ("return", "REGEX"),
                ("conditional", "true ? REGEX : null"),
                ("template", "`pattern ${REGEX}`"),
            ] {
                expressions.push((
                    format!("regex literal {literal} in {position}"),
                    context.replace("REGEX", literal),
                ));
            }
        }
        for (name, expression) in expressions {
            for form in ["expression", "return", "binding"] {
                let body = match form {
                    "return" => format!("{{ return {expression}; }}"),
                    "binding" => format!("{{ const value = {expression}; return value; }}"),
                    _ => expression.clone(),
                };
                cases.push(Case {
                    name: format!("FIG-7664 {name}, form={form}"),
                    source: format!("const run = async (): Promise<unknown> => {body};"),
                    cell: true,
                    materialize: true,
                    processes: false,
                    names: vec![],
                });
            }
        }
        let captions: serde_json::Value = serde_json::from_str(include_str!(
            "../../../lash-kernel-conformance/captions/typescript.json"
        ))
        .unwrap();
        cases.extend(serde_json::from_value::<Vec<Case>>(captions["readback"].clone()).unwrap());
        let mut failures = Vec::new();
        for case in &cases {
            let environment = Environment::new(
                library,
                if case.processes {
                    &process_namespaces
                } else {
                    &namespaces
                },
            );
            let mode = if case.cell {
                lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment::first())
            } else {
                lash_kernel_dialect::Mode::Module
            };
            let lowered = crate::lower(&case.source, &environment, mode)
                .unwrap_or_else(|error| panic!("{}: {error:?}", case.name));
            if case.materialize {
                let opened = materialize_callable(&lowered);
                let mut draft = lash_kernel_edit::Draft::open(opened.document.clone()).unwrap();
                if let Err(error) = draft.apply(
                    &lash_kernel_edit::Transaction {
                        base: opened.document.identity(),
                        edits: vec![],
                    },
                    &lash_kernel_check::Environment::new(library),
                    &[crate::CONVENTIONS],
                ) {
                    failures.push(format!(
                        "{}: saved zero-edit admission: {error:?}",
                        case.name
                    ));
                }
                // The publication printer checks the complete saved document, allowing
                // only the existing private-name and import-slot normalization.
                match print(
                    &opened.document,
                    Some(&opened.captions),
                    library,
                    PrintOptions {
                        source_map: true,
                        ..Default::default()
                    },
                ) {
                    Ok(saved) => {
                        let read = crate::lower(
                            &saved.source,
                            &environment,
                            lash_kernel_dialect::Mode::Module,
                        )
                        .unwrap_or_else(|error| {
                            panic!("{}: saved ordinary main readback: {error:?}", case.name)
                        });
                        let reprinted = print(
                            &read.document,
                            Some(&read.captions),
                            library,
                            Default::default(),
                        )
                        .unwrap();
                        assert_eq!(
                            reprinted.source, saved.source,
                            "{}: saved printing must stabilize",
                            case.name
                        );
                        saved
                            .source_map
                            .as_ref()
                            .unwrap()
                            .validate(&opened.document, &saved.source)
                            .unwrap();
                    }
                    Err(error) => {
                        failures.push(format!("{}: saved callable: {error:?}", case.name))
                    }
                }
            }
            let mut draft =
                lash_kernel_edit::Draft::open(lowered.document.document().clone()).unwrap();
            if let Err(error) = draft.apply(
                &lash_kernel_edit::Transaction {
                    base: lowered.document.id(),
                    edits: vec![],
                },
                &lash_kernel_check::Environment::new(library),
                &[crate::CONVENTIONS],
            ) {
                failures.push(format!("{}: zero-edit admission: {error:?}", case.name));
            }
            for name in &case.names {
                assert!(
                    lowered
                        .document
                        .functions
                        .values()
                        .any(|definition| definition.name.as_str() == name),
                    "{}: authored function {name} was lost",
                    case.name
                );
            }
            let printed = match print(
                &lowered.document,
                Some(&lowered.captions),
                library,
                PrintOptions {
                    source_map: true,
                    ..Default::default()
                },
            ) {
                Ok(printed) => printed,
                Err(error) => {
                    let reasons: Vec<_> = lift::document(
                        &lowered.document,
                        Some(&lowered.captions),
                        library,
                        STYLES[0],
                    )
                    .functions
                    .into_iter()
                    .filter_map(|(name, (_, item))| item.err().map(|why| (name, why)))
                    .collect();
                    failures.push(format!(
                        "{}: {error:?}, {reasons:?}\n{}",
                        case.name,
                        lash_kernel_doc::print_document(&lowered.document)
                    ));
                    continue;
                }
            };
            let read = crate::lower(&printed.source, &environment, mode)
                .unwrap_or_else(|error| panic!("{}: {error:?}\n{}", case.name, printed.source));
            let reprinted = print(
                &read.document,
                Some(&read.captions),
                library,
                PrintOptions {
                    source_map: true,
                    ..Default::default()
                },
            )
            .unwrap_or_else(|error| panic!("{}: second print: {error:?}", case.name));
            assert_eq!(
                reprinted.source, printed.source,
                "{}: readable printing must stabilize",
                case.name
            );
            if read.document.canonical_bytes() != lowered.document.canonical_bytes() {
                failures.push(format!(
                    "{}: identity mismatch\n{}",
                    case.name, printed.source
                ));
            }
            assert_eq!(
                read.captions.entries(),
                lowered.captions.entries(),
                "{}",
                case.name
            );
            printed
                .source_map
                .as_ref()
                .unwrap()
                .validate(&lowered.document, &printed.source)
                .unwrap();
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
        println!(
            "{} authored printer corpus cases round-tripped",
            cases.len()
        );
    }

    /// FIG-7323: copy-out and its optional builtin factory are implicit in
    /// the authored call, including inside an unawaited call's child task.
    #[test]
    fn forwarding_arguments_print_and_read_with_their_authored_call() {
        let library = crate::tests::library();
        let base = crate::tests::lower_with_effects("await host.echo(null);").unwrap();
        let namespaces = regions::namespaces(&base.document).unwrap();
        let environment = Environment::new(library, &namespaces);
        for body in [
            "return await host.echo(value);",
            "const pending = host.echo(value); return await pending;",
            "return await host.echo({f: Math.abs});",
            "const controller = new AbortController(); const pending = host.echo({f: Math.abs, value}, {signal: controller.signal}); controller.abort(); return await pending;",
        ] {
            let source = format!("export async function forward(value: any): any {{ {body} }}");
            let lowered =
                crate::lower(&source, &environment, lash_kernel_dialect::Mode::Module).unwrap();
            let printed = print(
                &lowered.document,
                Some(&lowered.captions),
                library,
                Default::default(),
            )
            .unwrap_or_else(|error| panic!("{source}: {error:?}"));
            let read = crate::lower(
                &printed.source,
                &environment,
                lash_kernel_dialect::Mode::Module,
            )
            .unwrap();
            assert_eq!(
                read.document.canonical_bytes(),
                lowered.document.canonical_bytes(),
                "{source}"
            );
            assert_eq!(read.captions.entries(), lowered.captions.entries());
        }
    }

    /// FIG-7279: an early return in an annotated module retains its exact
    /// document, captions and source map through ordinary source printing.
    #[test]
    fn an_early_return_module_prints_and_reads_identically() {
        let library = crate::tests::library();
        let namespaces = lash_kernel_dialect::Namespaces::default();
        let environment = Environment::new(library, &namespaces);
        let lowered = crate::lower(
            r#"/** Describe a number */
export async function demo(value: number): Promise<string> {
  /** Check for a negative value */
  if (value < 0) {
    /** Report that it is negative */
    return "negative";
  }
  /** Report that it is done */
  return "done";
}"#,
            &environment,
            lash_kernel_dialect::Mode::Module,
        )
        .unwrap();
        let printed = print(
            &lowered.document,
            Some(&lowered.captions),
            library,
            PrintOptions {
                source_map: true,
                ..Default::default()
            },
        )
        .unwrap();
        let read = crate::lower(
            &printed.source,
            &environment,
            lash_kernel_dialect::Mode::Module,
        )
        .unwrap();
        assert_eq!(
            read.document.canonical_bytes(),
            lowered.document.canonical_bytes()
        );
        assert_eq!(read.captions.entries(), lowered.captions.entries());
        printed
            .source_map
            .as_ref()
            .unwrap()
            .validate(&lowered.document, &printed.source)
            .unwrap();
    }

    /// FIG-7254: ordinary k names print and read with exact document identity,
    /// including a parameter whose generated local must avoid a user's k_1.
    #[test]
    fn k_bindings_print_and_read_identically() {
        let library = crate::tests::library();
        let namespaces = lash_kernel_dialect::Namespaces::default();
        let environment = Environment::new(library, &namespaces);
        let lowered = crate::lower(
            "export function k(k: number, k_1: number): number { return k + k_1; }",
            &environment,
            lash_kernel_dialect::Mode::Module,
        )
        .unwrap();
        let printed = print(&lowered.document, None, library, Default::default()).unwrap();
        assert!(printed.source.contains("function k("), "{}", printed.source);
        let (read, _) = read(
            &printed.source,
            &lowered.document,
            library,
            Default::default(),
            None,
        )
        .unwrap();
        assert_eq!(read.canonical_bytes(), lowered.document.canonical_bytes());
    }

    /// FIG-7176: a typed tool result's indexed field read inside try/catch
    /// retains its exact lowering when printed, including its source map.
    #[test]
    fn a_typed_tool_result_in_a_retry_loop_prints_and_reads_identically() {
        let input = serde_json::json!({"type":"object","properties":{
            "query":{"type":"string"},"limit":{"type":"integer"}
        },"required":["query"],"additionalProperties":false});
        let output = serde_json::json!({"type":"object","properties":{
            "results":{"type":"array","items":{"type":"object","properties":{
                "title":{"type":"string"},"url":{"type":"string"},"content":{"type":"string"}
            },"required":["title","url","content"],"additionalProperties":false}}
        },"required":["results"],"additionalProperties":false});
        let signature = lash_kernel_value::json_schema::tool_signature(
            &lash_kernel_value::JsonSchema::admit(input).unwrap(),
            &lash_kernel_value::JsonSchema::admit(output).unwrap(),
        );
        let namespaces = lash_kernel_dialect::Namespaces::new(
            [(
                lash_kernel_value::PackagePath::parse("web").unwrap(),
                [(
                    lash_kernel_value::Segment::new("search").unwrap(),
                    lash_kernel_dialect::NamespaceMember {
                        visibility: lash_kernel_value::Visibility::Public,
                        kind: lash_kernel_dialect::NamespaceKind::Operation(
                            lash_kernel_dialect::OperationSignature {
                                signature,
                                controls: lash_kernel_value::TurnControls::none(),
                                mode: lash_kernel_value::OperationMode::Waited,
                            },
                        ),
                    },
                )]
                .into(),
            )]
            .into(),
            Default::default(),
            &|_| false,
        )
        .unwrap();
        let library = crate::tests::library();
        let environment = Environment::new(library, &namespaces);
        let lowered = crate::lower(
            r#"export async function retry(): Promise<string> {
                for (const limit of [2, 50, 1]) {
                    try {
                        const found = await web.search({query: "workflow engines", limit});
                        return found.results[0].title;
                    } catch (error) {}
                }
                return "none";
            }"#,
            &environment,
            lash_kernel_dialect::Mode::Module,
        )
        .unwrap();
        let printed = super::print(
            &lowered.document,
            None,
            library,
            PrintOptions {
                source_map: true,
                ..Default::default()
            },
        )
        .unwrap();
        let (read, _) = super::read(
            &printed.source,
            &lowered.document,
            library,
            Default::default(),
            None,
        )
        .unwrap();
        assert_eq!(read.canonical_bytes(), lowered.document.canonical_bytes());
        printed
            .source_map
            .unwrap()
            .validate(&read, &printed.source)
            .unwrap();
    }
}
