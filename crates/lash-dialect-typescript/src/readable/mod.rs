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
mod style;
mod switches;
mod updates;

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
    let mut printed = print_attempt(document, captions, library, options, style::STYLES[0]);
    // Exact operation spellings come last: only a document whose convenient
    // reads and method calls do not read back is printed with them.
    for style in style::attempts().skip(1) {
        if !matches!(printed, Err(PrintError::Unprintable { .. })) {
            break;
        }
        printed = print_attempt(document, captions, library, options, style);
    }
    printed
}

fn print_attempt(
    document: &k::Document,
    captions: Option<&k::Captions>,
    library: &dyn Library,
    options: PrintOptions,
    style: style::Style,
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
    /// evidence it is unprintable. A callable capturing a declared binding
    /// carries its declared type (FIG-7710).
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
        // FIG-7710: the declared type of a binding a callable captures is the
        // capturing callable's evidence too; the materialized capture cell
        // declares it again.
        let captured = crate::lower(
            "const us: unknown[] = [1, 2];\nconst annotated = async (): Promise<unknown> => us[1];\n",
            &environment,
            lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment::first()),
        )
        .unwrap();
        let filled = |mut function: k::FunctionValue| {
            let int = |i: i64| k::CaptureValue::Data(k::Datum::Int(i.into()));
            function.captures = [(
                lash_kernel_value::Name::new("us"),
                k::CaptureValue::List(vec![int(1), int(2)]),
            )]
            .into();
            function
        };
        reads_back(&materialize_callable_as(&captured, filled)).unwrap();
        let refused = reads_back(&materialize_callable_as(&captured, |function| {
            let mut function = filled(function);
            function.declared_locals.clear();
            function
        }));
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

    /// One of the enumeration law's forms with `x` a known receiver.
    struct KnownReceiver {
        kind: &'static str,
        value: k::CaptureValue,
        /// The form's own declarations, then `const x = <literal>;`.
        prefix: String,
        body: String,
    }

    /// The enumeration law's forms over `x` alone (or `x` and an untyped
    /// `y`), with `x` a known list, map, set, record and text.
    fn known_receiver_forms() -> Vec<KnownReceiver> {
        let int = |i: i64| k::CaptureValue::Data(k::Datum::Int(i.into()));
        let text = |t: &str| k::CaptureValue::Data(k::Datum::Text(t.into()));
        let kinds = [
            (
                "list",
                k::CaptureValue::List(vec![int(7), int(8)]),
                "[7, 8]",
            ),
            (
                "map",
                k::CaptureValue::Map(vec![(text("a"), int(1))]),
                "new Map([[\"a\", 1]])",
            ),
            ("set", k::CaptureValue::Set(vec![int(1)]), "new Set([1])"),
            (
                "record",
                k::CaptureValue::Record(vec![("a".into(), int(5)), ("b".into(), text("z"))]),
                "{a: 5, b: \"z\"}",
            ),
            ("text", text("78"), "\"78\""),
        ];
        let forms = crate::lower::inventory::forms()
            .into_iter()
            .chain(crate::lower::inventory::members());
        let mut out: Vec<KnownReceiver> = Vec::new();
        for (_, source) in forms {
            let Some(start) = source.find("export function demo(") else {
                continue;
            };
            let rest = &source[start + "export function demo(".len()..];
            let Some(close) = rest.find(')') else {
                continue;
            };
            if rest[..close]
                .split(',')
                .map(str::trim)
                .any(|param| !(param.is_empty() || param.starts_with("x:") || param == "y: any"))
            {
                continue;
            }
            let (Some(open), Some(end)) = (rest.find('{'), rest.rfind('}')) else {
                continue;
            };
            let body = rest[open + 1..end].trim();
            for (kind, value, literal) in &kinds {
                let prefix = format!("{}const x = {literal};\n", &source[..start]);
                // Annotated variants of a form share its body.
                if out
                    .iter()
                    .any(|form| form.kind == *kind && form.prefix == prefix && form.body == body)
                {
                    continue;
                }
                out.push(KnownReceiver {
                    kind,
                    value: value.clone(),
                    prefix,
                    body: body.to_owned(),
                });
            }
        }
        out
    }

    const ALL: &[&str] = &["list", "map", "record", "set", "text"];

    /// Captured forms applying an operator to the capture itself, which the
    /// original reads in place as a value of its known kind: they print
    /// through the operand spellings (`value.operand(x)`, `list.operand`,
    /// `record.operand`, `value.text_operand`), and templates over it, which
    /// the original joins unconverted, through `value.text_join`. Each must
    /// lower, materialize, print and import: a skip is a failure, not an
    /// acceptance.
    const OPERAND_FORMS: &[(&[&str], &str)] = &[
        (ALL, "const {a = 1, ...rest} = x; return rest;"),
        (ALL, "const {a, b: [c]} = x; return a + c;"),
        (ALL, "const {a, ...rest} = x; return rest;"),
        (ALL, "delete x.a; return x;"),
        (
            ALL,
            "for (const key in x) { if (key) return key; } return null;",
        ),
        (ALL, "return \"a\" in x;"),
        (ALL, "return 'a' in x;"),
        (ALL, "return delete x.a;"),
        (ALL, "return delete x?.a;"),
        (ALL, "return new x(1);"),
        (ALL, "return x(...[1, 2]);"),
        (ALL, "return x?.(1);"),
        (ALL, "return x?.[1];"),
        (ALL, "return x?.a?.(1);"),
        (ALL, "return x?.a;"),
        (ALL, "return x in y;"),
        (&["record"], "return x.f(...[1, 2]);"),
        (&["text"], "return `a\\n${x}\\`${x}`;"),
        (&["text"], "return `hello ${x}!`;"),
    ];

    /// FIG-7711: every enumerated form over a known receiver that lowers
    /// prints and reads back to the same document, in a plain module and as
    /// a published arrow capturing `x`. The [`OPERAND_FORMS`] print, import, and run as
    /// the original runs: with fills of another kind, with a later operand
    /// that mutates the receiver, and with optional-chain operands that
    /// throw when evaluated. The operand spellings keep the operator's
    /// helper identity: a missing or re-identified helper and a namespace
    /// claiming the spelling's root refuse, and a template's join keeps
    /// `text.join`'s identity and adds no conversion.
    #[test]
    fn enumerated_forms_over_known_receivers_print() {
        use k::CaptureValue as C;
        let library = crate::tests::library();
        let namespaces = Default::default();
        let environment = Environment::new(library, &namespaces);
        let mut refusals = Vec::new();
        let mut operand_forms = BTreeSet::new();
        let cell_mode =
            || lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment::first());
        let fill = |value: C| {
            move |mut function: k::FunctionValue| {
                function.captures = [(lash_kernel_value::Name::new("x"), value.clone())]
                    .into_iter()
                    .collect();
                function
            }
        };
        // The printed callable imports as an ordinary module and prints
        // again to the same source.
        let imports = |printed: &PrintedSource| -> Result<lash_kernel_dialect::Lowered, String> {
            let read = crate::lower(
                &printed.source,
                &environment,
                lash_kernel_dialect::Mode::Module,
            )
            .map_err(|error| format!("import: {error}"))?;
            let reprinted = print(
                &read.document,
                Some(&read.captions),
                library,
                Default::default(),
            )
            .map_err(|error| format!("reprint: {error}"))?;
            if reprinted.source != printed.source {
                return Err(format!("reprints as\n{}", reprinted.source));
            }
            Ok(read)
        };
        for form in known_receiver_forms() {
            let KnownReceiver {
                kind,
                value,
                prefix,
                body,
            } = &form;
            let operand = OPERAND_FORMS
                .iter()
                .any(|(kinds, form)| *form == body && kinds.contains(kind));
            let module = format!(
                "{prefix}export async function demo(y: any): Promise<unknown> {{ {body} }}\n"
            );
            if let Ok(lowered) =
                crate::lower(&module, &environment, lash_kernel_dialect::Mode::Module)
                && let Err(error) = print(
                    &lowered.document,
                    Some(&lowered.captions),
                    library,
                    Default::default(),
                )
            {
                refusals.push(format!("module {kind}: {body} :: {error}"));
            }
            let cell =
                format!("{prefix}const demo = async (y: any): Promise<unknown> => {{ {body} }};\n");
            let lowered = match crate::lower(&cell, &environment, cell_mode()) {
                Ok(lowered) => lowered,
                Err(error) if operand => {
                    refusals.push(format!("captured {kind}: {body} :: lower: {error}"));
                    continue;
                }
                Err(_) => continue,
            };
            // The materializer fills `x` alone: a callable that also
            // captures other cell values (a tagged template's strings) is
            // outside the captured arm.
            let Ok(opened) =
                std::panic::catch_unwind(|| materialize_callable_as(&lowered, fill(value.clone())))
            else {
                if operand {
                    refusals.push(format!("captured {kind}: {body} :: materialization"));
                }
                continue;
            };
            match print(
                &opened.document,
                Some(&opened.captions),
                library,
                Default::default(),
            ) {
                Err(error) => refusals.push(format!("captured {kind}: {body} :: {error}")),
                Ok(printed) if operand => {
                    operand_forms.insert((*kind, body.clone()));
                    if let Err(error) = imports(&printed) {
                        refusals.push(format!("captured {kind}: {body} :: {error}"));
                    }
                }
                Ok(_) => {}
            }
        }
        for (kinds, body) in OPERAND_FORMS {
            for kind in *kinds {
                if !operand_forms.contains(&(*kind, (*body).to_owned())) {
                    refusals.push(format!("captured {kind}: {body} :: not enumerated"));
                }
            }
        }
        assert!(
            refusals.is_empty(),
            "{} refusals:\n{}",
            refusals.len(),
            refusals.join("\n")
        );

        // The printed callable runs as the original does, `y` its input: a
        // result, or the same failure in the same function.
        let int = |i: i64| C::Data(k::Datum::Int(i.into()));
        let text = |t: &str| C::Data(k::Datum::Text(t.into()));
        let list = || C::List(vec![int(7), int(8)]);
        let record = || C::Record(vec![("a".into(), int(5)), ("b".into(), text("z"))]);
        let null = || C::Data(k::Datum::Null);
        const LIST: &str = "[7, 8]";
        const RECORD: &str = "{a: 5, b: \"z\"}";
        const TEXT: &str = "\"78\"";
        // A function is named by its export, else its own name: the
        // materialized entry is the printed module's `run`.
        let outcome = |document: k::Document, y: &k::Datum| {
            let names: Vec<(String, String)> = document
                .functions
                .iter()
                .map(|(id, function)| {
                    let exported = document.exports.iter().find_map(|(name, export)| {
                        matches!(export, k::Export::Function(entry) if entry.function == *id)
                            .then(|| name.to_string())
                    });
                    (
                        format!("{id:?}"),
                        exported.unwrap_or_else(|| function.name.to_string()),
                    )
                })
                .collect();
            let (asked, end) =
                crate::tests::machine::outcome_of_entry(document, "run", vec![y.clone()]);
            let mut ended = format!("{asked:?} {end:?}");
            for (id, name) in names {
                ended = ended.replace(&id, &name);
            }
            (end.ok(), ended)
        };
        let reading_k = "Cannot read properties of undefined or null (reading 'k')";
        // `text.join`'s own check of its members.
        const JOIN_REFUSAL: &str = "join requires text members";
        // A publication reads back through the release its document pins:
        // with the original helper retained beside a newer one under the
        // same name, the document reads back to the original's identity;
        // once the original is retired, it is refused, never read back to
        // the newer helper the name now resolves to.
        fn retained_reads_back(
            document: &k::Document,
            newer: &crate::tests::helper::Helper<'_>,
            original: Option<lash_kernel_value::FunctionId>,
        ) {
            let original = original.expect("the original helper");
            assert_ne!(newer.id, Some(original));
            let (canonical, _) = crate::constructs::conventions::CONVENTIONS
                .verify_inverse(document, newer)
                .expect("the retained release reads back");
            // The readback names the original helper's identity, never the
            // newer one the name now resolves to.
            let bytes = canonical.canonical_bytes();
            let names = |id: &lash_kernel_value::FunctionId| {
                bytes.windows(32).any(|window| window == id.as_bytes())
            };
            assert!(names(&original));
            assert!(!names(&newer.id.expect("the newer helper")));
            let retired = crate::tests::helper::Helper {
                retired: Some(original),
                ..*newer
            };
            let refused =
                crate::constructs::conventions::CONVENTIONS.verify_inverse(document, &retired);
            assert!(
                refused.is_err(),
                "{:?}",
                refused.map(|(read, _)| read.identity())
            );
        }
        let record_y = k::Datum::Record(vec![("7,8,9".into(), k::Datum::Bool(true))]);
        // (x's literal, its fill, body, y, the result JavaScript gives or
        // the failure message it raises, where the case has one).
        type Expected<'a> = Option<Result<k::Datum, &'a str>>;
        let cases: Vec<(&str, C, &str, k::Datum, Expected)> = vec![
            // Fills of another kind than the original's: the kind's own
            // operation fails as the original's does.
            (LIST, record(), "return x?.[1];", k::Datum::Null, None),
            (LIST, text("78"), "return x?.a;", k::Datum::Null, None),
            (RECORD, list(), "return x?.a;", k::Datum::Null, None),
            (
                RECORD,
                list(),
                "const {a, ...rest} = x; return rest;",
                k::Datum::Null,
                None,
            ),
            (
                LIST,
                text("78"),
                "const {a, b: [c]} = x; return a + c;",
                k::Datum::Null,
                None,
            ),
            (TEXT, int(3), "return x?.[1];", k::Datum::Null, None),
            (LIST, text("q"), "return \"a\" in x;", k::Datum::Null, None),
            (
                RECORD,
                list(),
                "return x.f(...[1, 2]);",
                k::Datum::Null,
                None,
            ),
            // A later operand mutates the receiver the operator read in
            // place: the deleted slot is the pushed one, and `in` converts
            // the mutated list to its key.
            (
                LIST,
                list(),
                "return [delete x[(x.push(9), 2)], 2 in x, x.length];",
                k::Datum::Null,
                Some(Ok(k::Datum::List(vec![
                    k::Datum::Bool(true),
                    k::Datum::Bool(false),
                    k::Datum::Float(k::Float::new(3.0)),
                ]))),
            ),
            (
                LIST,
                list(),
                "return x in (x.push(9), y);",
                record_y.clone(),
                Some(Ok(k::Datum::Bool(true))),
            ),
            // Optional-chain operands that throw when evaluated: skipped on
            // a nullish base, evaluated before the call's own check
            // otherwise.
            (
                RECORD,
                null(),
                "return x?.a?.(y.k);",
                k::Datum::Null,
                Some(Ok(k::Datum::Null)),
            ),
            (
                RECORD,
                null(),
                "return x?.(y.k);",
                k::Datum::Null,
                Some(Ok(k::Datum::Null)),
            ),
            (
                RECORD,
                record(),
                "return x?.a?.(y.k);",
                k::Datum::Null,
                Some(Err(reading_k)),
            ),
            (
                RECORD,
                record(),
                "return x?.(y.k);",
                k::Datum::Null,
                Some(Err(reading_k)),
            ),
            (
                RECORD,
                record(),
                "return new x(y.k);",
                k::Datum::Null,
                Some(Err(reading_k)),
            ),
            (
                RECORD,
                record(),
                "return x.f(...[y.k]);",
                k::Datum::Null,
                Some(Err(reading_k)),
            ),
            (
                TEXT,
                null(),
                "return x?.[1];",
                k::Datum::Null,
                Some(Ok(k::Datum::Null)),
            ),
            // A template over a text joins it as it is, escapes kept; one
            // over another kind fails the join's text check where the
            // original's does, with no conversion to succeed by.
            (
                TEXT,
                text("q"),
                "return `a\\n${x}\\`${x}`;",
                k::Datum::Null,
                Some(Ok(k::Datum::Text("a\nq`q".into()))),
            ),
            (
                TEXT,
                int(3),
                "return `hello ${x}!`;",
                k::Datum::Null,
                Some(Err(JOIN_REFUSAL)),
            ),
            (
                TEXT,
                list(),
                "return `a\\n${x}\\`${x}`;",
                k::Datum::Null,
                Some(Err(JOIN_REFUSAL)),
            ),
            // A part the original converts keeps its conversion.
            (
                TEXT,
                text("q"),
                "return `${x}-${y}`;",
                k::Datum::Int(3i64.into()),
                Some(Ok(k::Datum::Text("q-3".into()))),
            ),
        ];
        for (literal, value, body, y, expected) in cases {
            let cell = format!(
                "const x = {literal};\nconst demo = async (y: any): Promise<unknown> => {{ {body} }};\n"
            );
            let lowered = crate::lower(&cell, &environment, cell_mode()).unwrap();
            let opened = materialize_callable_as(&lowered, fill(value.clone()));
            let printed = print(
                &opened.document,
                Some(&opened.captions),
                library,
                Default::default(),
            )
            .unwrap_or_else(|error| panic!("{body} :: {error}"));
            let read = imports(&printed).unwrap_or_else(|error| panic!("{body} :: {error}"));
            let (result, original) = outcome(opened.document.clone(), &y);
            let (_, reimported) = outcome(read.document.into_document(), &y);
            assert_eq!(
                original, reimported,
                "{body} filled with {value:?}\n{}",
                printed.source
            );
            match expected {
                Some(Ok(expected)) => assert_eq!(result, Some(expected), "{body}"),
                Some(Err(message)) => {
                    assert!(
                        result.is_none() && original.contains(message),
                        "{body}: {original}"
                    )
                }
                None => {}
            }
        }

        // `"a" in value.operand(x)` lowers to the `in` operator's helper,
        // `ts.in`, by its installed identity.
        let lowered = crate::lower(
            &format!(
                "const x = {LIST};\nconst demo = async (y: any): Promise<unknown> => {{ return \"a\" in x; }};\n"
            ),
            &environment,
            cell_mode(),
        )
        .unwrap();
        let opened = materialize_callable_as(&lowered, fill(list()));
        let printed = print(
            &opened.document,
            Some(&opened.captions),
            library,
            Default::default(),
        )
        .unwrap();
        assert!(
            printed.source.contains("\"a\" in value.operand(x)"),
            "{}",
            printed.source
        );
        let missing = crate::tests::helper::Helper {
            library,
            name: "ts.in",
            id: None,
            retired: None,
        };
        let refused = crate::lower(
            &printed.source,
            &Environment::new(&missing, &namespaces),
            lash_kernel_dialect::Mode::Module,
        )
        .map(|_| ())
        .unwrap_err();
        assert_eq!(
            refused.code(),
            crate::DiagnosticCode::LinkError,
            "{refused}"
        );
        let refused = print(
            &opened.document,
            Some(&opened.captions),
            &missing,
            Default::default(),
        );
        assert!(
            matches!(refused, Err(PrintError::Unprintable { .. })),
            "{refused:?}"
        );
        // The same name for another helper is not the original's identity.
        let renamed = crate::tests::helper::Helper {
            library,
            name: "ts.in",
            id: library.resolve("ts.has"),
            retired: None,
        };
        assert!(renamed.id.is_some());
        let refused = print(
            &opened.document,
            Some(&opened.captions),
            &renamed,
            Default::default(),
        );
        assert!(
            matches!(refused, Err(PrintError::Unprintable { .. })),
            "{refused:?}"
        );
        retained_reads_back(&opened.document, &renamed, library.resolve("ts.in"));
        // A template's join keeps `text.join`'s identity: the printed
        // callable spells it, and a library naming another helper so does
        // not print it.
        let lowered = crate::lower(
            &format!(
                "const x = {TEXT};\nconst demo = async (y: any): Promise<unknown> => {{ return `hello ${{x}}!`; }};\n"
            ),
            &environment,
            cell_mode(),
        )
        .unwrap();
        let joined = materialize_callable_as(&lowered, fill(text("q")));
        let printed = print(
            &joined.document,
            Some(&joined.captions),
            library,
            Default::default(),
        )
        .unwrap();
        assert!(
            printed
                .source
                .contains("value.text_join([\"hello \", x, \"!\"], \"\")"),
            "{}",
            printed.source
        );
        let renamed = crate::tests::helper::Helper {
            library,
            name: "text.join",
            id: library.resolve("text.concat"),
            retired: None,
        };
        assert!(renamed.id.is_some());
        let refused = print(
            &joined.document,
            Some(&joined.captions),
            &renamed,
            Default::default(),
        );
        assert!(
            matches!(refused, Err(PrintError::Unprintable { .. })),
            "{refused:?}"
        );
        retained_reads_back(&joined.document, &renamed, library.resolve("text.join"));
        // No package claims the spelling's root.
        let claimed = lash_kernel_dialect::Namespaces::new(
            Default::default(),
            [lash_kernel_value::names::PackagePath::new(
                lash_kernel_value::Segment::new("value").unwrap(),
            )]
            .into(),
            &lash_kernel_dialect::value_constructors::Constructor::reserves,
        );
        assert!(
            matches!(
                claimed,
                Err(lash_kernel_dialect::NamespaceRefusal::ReservedRoot { ref root, .. }) if root == "value"
            ),
            "{claimed:?}"
        );
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
                    .iter()
                    .all(|param| param.name.as_str() == "y")
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
                k::Datum::Float(k::Float::new(presentation.signature.params.len() as f64)),
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
            ("views", "text"),
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
                        style::STYLES[0],
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
