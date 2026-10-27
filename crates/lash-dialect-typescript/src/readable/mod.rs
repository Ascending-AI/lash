//! Readable export: a document printed as the TypeScript a model would
//! write, verified region by region.
//!
//! Every printed module must lower to the complete original document and
//! captions. If authored source cannot express it, printing refuses.

mod closures;
mod expressions;
mod lift;
mod methods;
mod mixed;
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
    for style in STYLES[1..]
        .iter()
        .copied()
        .chain(STYLES.iter().map(|style| lift::Style {
            const_functions: true,
            ..*style
        }))
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
        const_functions: false,
        operators: true,
        preserve_temporaries: false,
    },
    lift::Style {
        const_functions: false,
        operators: true,
        preserve_temporaries: true,
    },
    lift::Style {
        const_functions: false,
        operators: false,
        preserve_temporaries: false,
    },
    lift::Style {
        const_functions: false,
        operators: false,
        preserve_temporaries: true,
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
                    .map_err(|_| PrintError::Unprintable {
                        site: lash_kernel_value::sites::Site::new(
                            lash_kernel_value::sites::Unit::Library(function),
                            Vec::new(),
                        ),
                    })
            })
            .collect::<Result<_, _>>()?,
        main: Some(lifted.main.map_err(|_| PrintError::Unprintable {
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
        .map_err(|diagnostic| diagnostic.span.map(|span| span.start))
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
            names: Vec<String>,
        }
        let library = crate::tests::library();
        let mut packages = BTreeMap::new();
        for (package, member) in [
            ("people", "lookup"),
            ("inbox", "send_item"),
            ("workflows", "create"),
            ("corpus", "echo"),
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
                                    params: vec![lash_kernel_value::types::Param {
                                        name: Name::new("input"),
                                        ty: lash_kernel_value::types::Type::Any,
                                        optional: false,
                                    }],
                                    result: lash_kernel_value::types::Type::Any,
                                },
                                controls: lash_kernel_value::TurnControls::none(),
                                mode: lash_kernel_value::OperationMode::Waited,
                            },
                        ),
                    },
                );
        }
        let namespaces =
            lash_kernel_dialect::Namespaces::new(packages, Default::default(), &|_| false).unwrap();
        let environment = Environment::new(library, &namespaces);
        let mut cases: Vec<Case> = serde_json::from_str(include_str!("corpus.json")).unwrap();
        let captions: serde_json::Value = serde_json::from_str(include_str!(
            "../../../lash-kernel-conformance/captions/typescript.json"
        ))
        .unwrap();
        cases.extend(serde_json::from_value::<Vec<Case>>(captions["readback"].clone()).unwrap());
        let mut failures = Vec::new();
        for case in &cases {
            let mode = if case.cell {
                lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment::first())
            } else {
                lash_kernel_dialect::Mode::Module
            };
            let lowered = crate::lower(&case.source, &environment, mode)
                .unwrap_or_else(|error| panic!("{}: {error:?}", case.name));
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
                    failures.push(format!("{}: {error:?}", case.name));
                    continue;
                }
            };
            let read = crate::lower(&printed.source, &environment, mode)
                .unwrap_or_else(|error| panic!("{}: {error:?}\n{}", case.name, printed.source));
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
