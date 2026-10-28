//! FIG-6384 / Design 8: readable, per-dialect workflow and cell witnesses.
//! The catalog is also the host example's source inventory. Generating an
//! output checks its semantic laws before any text can be blessed.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use lash_kernel_check::{Environment as Checker, WorkflowDocument};
use lash_kernel_dialect::{CellEnvironment, Environment, Lowered, Mode, PrintOptions};
use lash_kernel_doc::{self as k, Captions, Document, Expr, Node, SourceLanguage};
use lash_kernel_edit::{Draft, Transaction};
use lash_kernel_value::names::Name;
use lash_kernel_value::sites::Site;
use lash_kernel_value::sites::Unit;
use serde::{Deserialize, Serialize};

mod cell;
mod namespace;
use namespace::FixtureNamespace;
use {lash_kernel_value::names::LinkedStart, lash_kernel_value::names::StartSelector};
mod printed;
mod readable;

pub use printed::{Lifted, SCAFFOLDING, readable_prints};

/// A corpus case: dialect-specific source, with a shared entry/refusal contract.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub name: String,
    pub mode: CaseMode,
    /// A stable module-contract code, never dialect diagnostic prose.
    #[serde(default)]
    pub refusal: Option<String>,
    #[serde(default)]
    pub entries: Vec<String>,
    pub dialects: Vec<String>,
    /// Synthetic kinds allowed to lack source ranges, declared per dialect.
    pub exceptions: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub edit: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseMode {
    Module,
    Cell,
}

/// Read the same catalog the workflow example consumes.
#[expect(
    clippy::disallowed_methods,
    reason = "corpus tooling reads explicitly supplied fixture paths"
)]
pub fn catalog(root: &Path) -> Result<Vec<Case>> {
    Ok(serde_json::from_str(&std::fs::read_to_string(
        root.join("catalog.json"),
    )?)?)
}

fn language(extension: &str) -> Result<SourceLanguage> {
    match extension {
        "ts" => Ok(SourceLanguage::TypeScript),
        "py" => Ok(SourceLanguage::Python),
        _ => anyhow::bail!("unknown corpus dialect {extension}"),
    }
}

fn lower(
    source: &str,
    lang: SourceLanguage,
    env: &Environment<'_>,
    mode: Mode<'_>,
) -> Result<Lowered, lash_kernel_dialect::Diagnostic> {
    match lang {
        SourceLanguage::TypeScript => {
            lash_dialect_typescript::lower(source, env, mode).map_err(Into::into)
        }
        SourceLanguage::Python => lash_dialect_python::lower(source, env, mode),
    }
}

/// What printing a corpus document checks itself against: the library its
/// dialect helpers resolve in and the environment it was imported under.
pub(crate) struct Printing<'a> {
    pub(crate) environment: &'a Environment<'a>,
    pub(crate) mode: Mode<'a>,
}

fn printed(
    document: &Document,
    captions: &Captions,
    lang: SourceLanguage,
    printing: &Printing<'_>,
) -> Result<lash_kernel_dialect::PrintedSource> {
    let options = PrintOptions {
        source_map: true,
        ..Default::default()
    };
    let output = match lang {
        SourceLanguage::TypeScript => lash_dialect_typescript::print(
            document,
            Some(captions),
            printing.environment.library,
            options,
        )?,
        SourceLanguage::Python => lash_dialect_python::print(
            document,
            Some(captions),
            printing.environment.library,
            options,
        )?,
    };
    // Both dialects spell every captionable kind (K-CAP-006).
    ensure!(
        output.dropped.is_empty(),
        "the printer dropped captions at {:?}",
        output.dropped
    );
    Ok(output)
}

fn roundtrip(
    document: &Document,
    captions: &Captions,
    lang: SourceLanguage,
    printing: &Printing<'_>,
) -> Result<String> {
    let output = printed(document, captions, lang, printing)?;
    let (decoded, read) = match lang {
        SourceLanguage::TypeScript => lash_dialect_typescript::read(
            &output.source,
            document,
            printing.environment.library,
            Default::default(),
            None,
        )?,
        SourceLanguage::Python => lash_dialect_python::read(
            &output.source,
            document,
            printing.environment.library,
            Default::default(),
            None,
        )?,
    };
    ensure!(
        decoded.canonical_bytes() == document.canonical_bytes(),
        "authored import -> print -> read changed canonical bytes"
    );
    ensure!(
        read == *captions,
        "print -> read changed the captions (K-CAP-002)"
    );
    printed::laws(document, captions, &output.source, lang, printing)?;
    output
        .source_map
        .context("SM-ORDER: printer returns a requested map")?
        .validate(document, &output.source)?;
    // SM-ORDER's editor handoff accepts precisely the printed document.
    Draft::open(document.clone())?;
    Ok(output.source)
}

/// How many captions `source` spells: each TypeScript JSDoc block, and
/// each run of Python `#:` lines.
fn written_captions(source: &str, lang: SourceLanguage) -> usize {
    match lang {
        SourceLanguage::TypeScript => source.matches("/**").count(),
        SourceLanguage::Python => {
            let mut previous = false;
            source
                .lines()
                .filter(|line| {
                    let caption = line.trim_start().starts_with("#:");
                    let starts = caption && !previous;
                    previous = caption;
                    starts
                })
                .count()
        }
    }
}

pub(crate) fn code_sites(document: &Document, library: &dyn k::FunctionCatalog) -> Vec<Site> {
    fn walk(node: Node<'_>, site: Site, out: &mut Vec<Site>) {
        if matches!(
            node,
            Node::Stmt(_) | Node::Action(_) | Node::Expr(Expr::Act(_))
        ) {
            out.push(site.clone());
        }
        for (index, child) in (0u32..).zip(node.children()) {
            walk(child, site.child(index), out);
        }
    }
    let mut sites = Vec::new();
    for (unit, body) in document.units() {
        if matches!(&unit, Unit::Library(id) if library.definition(id).is_some()) {
            continue;
        }
        walk(Node::Block(body), Site::new(unit, vec![]), &mut sites);
    }
    sites
}

/// The corpus and the example offer precisely this tool declaration. Its
/// module/operation path is the public facade import contract.
pub fn tools() -> Result<Vec<lash::tools::ToolDefinition>> {
    use lash::tools::ToolDefinition;
    Ok(vec![
        ToolDefinition::raw(
            "tool:corpus_echo",
            "corpus.echo",
            "Echo a corpus value and display it.",
            serde_json::json!({}),
            ToolDefinition::any_output(),
        )?
        .with_execution(std::time::Duration::from_secs(120)),
        ToolDefinition::raw(
            "tool:corpus_capture",
            "corpus.capture",
            "Capture a function nested in a corpus value and display it.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "list": {
                        "type": "array",
                        "items": { "x-lash": { "kind": "function" } }
                    }
                },
                "required": ["list"],
                "additionalProperties": false
            }),
            // The capture harness acknowledges with null; the live example
            // echoes its input record after displaying it.
            serde_json::json!({"anyOf":[{"type":"null"},{"type":"object","properties":{"list":{"type":"array","items":{}}},"required":["list"]}]}),
        )?
        .with_execution(std::time::Duration::from_secs(120)),
    ])
}

/// Source-only corpus operation fixtures, projected from explicit Packages.
pub fn corpus_namespaces(
    effects: &BTreeMap<lash_kernel_value::EffectName, lash_kernel_value::Signature>,
    controls: &BTreeMap<
        lash_kernel_value::EffectName,
        BTreeSet<lash_kernel_dialect::EffectControl>,
    >,
) -> Result<lash_kernel_dialect::Namespaces> {
    let requirements = effects
        .iter()
        .map(|(name, signature)| {
            (
                name.clone(),
                lash_kernel_value::OperationRequirement::new(
                    signature.clone(),
                    lash_kernel_value::ContractId::new("1"),
                ),
            )
        })
        .collect();
    let controls = controls
        .iter()
        .map(|(name, kinds)| {
            let mut controls = lash_kernel_value::TurnControls::none();
            for kind in kinds {
                controls = match kind {
                    lash_kernel_dialect::EffectControl::Finish => {
                        controls.with_finish(lash_kernel_value::JsonSchema::any())
                    }
                    lash_kernel_dialect::EffectControl::SwitchAgentFrame => {
                        controls.with_switch_agent_frame()
                    }
                };
            }
            (name.clone(), controls)
        })
        .collect();
    FixtureNamespace::new(requirements, &controls)?.namespaces()
}

/// Generate review artifacts, checking reproducible import, exact source
/// reading, SM-ORDER/SM-COVER, closed edit replay and cell crossing structure.
/// Only entry names and refusal codes are compared between dialects.
pub fn render(root: &Path) -> Result<BTreeMap<String, String>> {
    render_with(root).map(|(outputs, _)| outputs)
}

/// [`render`], and how each document and its canvas edit printed.
#[expect(
    clippy::disallowed_methods,
    reason = "corpus tooling reads explicitly supplied fixture paths"
)]
pub(crate) fn render_with(root: &Path) -> Result<(BTreeMap<String, String>, Vec<Lifted>)> {
    let mut report = Vec::new();
    let embedding = lash_vm_worker::built_embedding(&Default::default())?;
    let registry = embedding.registry()?;
    let mut effects = BTreeMap::new();
    for definition in tools()? {
        effects.insert(
            definition.operation().effect_name(),
            lash_kernel_value::OperationRequirement::new(
                lash_kernel_value::json_schema::tool_signature(
                    &definition.contract.input_schema.canonical,
                    &definition.contract.output_schema.canonical,
                ),
                definition.contract_id(),
            ),
        );
    }
    // The original source-shape probes name these host effects. They are
    // fixture declarations; the example's offered toolbox stays in tools().
    for name in [
        "display.show_message",
        "display.set_status",
        "display.add_item",
        "gmail.list_recent",
        "llm.query",
        "web.search",
    ] {
        effects.insert(
            lash_kernel_value::EffectName::new(name)?,
            lash_kernel_value::OperationRequirement::new(
                lash_kernel_value::Signature {
                    params: vec![lash_kernel_value::Param {
                        name: "input".into(),
                        ty: lash_kernel_value::Type::Any,
                        optional: false,
                    }],
                    result: lash_kernel_value::Type::Any,
                },
                lash_kernel_value::ContractId::new("1"),
            ),
        );
    }
    // Finish belongs only to cells; it is not a process tool offered by the example.
    effects.insert(
        lash_kernel_value::EffectName::new("control.finish")?,
        lash_kernel_value::OperationRequirement::new(
            lash_kernel_value::Signature {
                params: vec![lash_kernel_value::Param {
                    name: Name::new("input"),
                    ty: lash_kernel_value::Type::Any,
                    optional: false,
                }],
                result: lash_kernel_value::Type::Any,
            },
            lash_kernel_value::ContractId::new("1"),
        ),
    );
    let controls = BTreeMap::from([(
        lash_kernel_value::EffectName::new("control.finish")?,
        lash_kernel_value::TurnControls::finish(lash_kernel_value::JsonSchema::any()),
    )]);
    let namespace = FixtureNamespace::new(effects, &controls)?;
    let env_namespaces = namespace.namespaces()?;
    let env = Environment::new(embedding.library(), &env_namespaces);
    let checker = Checker::new(registry.as_ref());
    let mut outputs = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for case in catalog(root)? {
        ensure!(
            seen.insert(case.name.clone()),
            "duplicate case {}",
            case.name
        );
        for extension in &case.dialects {
            let lang = language(extension)?;
            let source =
                std::fs::read_to_string(root.join(&case.name).join(format!("source.{extension}")))?;
            let mode = if case.mode == CaseMode::Module {
                Mode::Module
            } else {
                Mode::Cell(CellEnvironment::first())
            };
            let lowered = lower(&source, lang, &env, mode);
            let prefix = format!("{}/", case.name);
            if let Some(expected) = &case.refusal {
                let Err(diagnostic) = lowered else {
                    anyhow::bail!(
                        "MODULE-CONTRACT: {} {extension}: refused case must refuse ({expected})",
                        case.name
                    );
                };
                ensure!(
                    diagnostic.code() == expected,
                    "{} {extension}: expected {expected}, got {diagnostic}",
                    case.name
                );
                outputs.insert(
                    format!("{prefix}refusal.{extension}.txt"),
                    format!("code: {}\n", diagnostic.code()),
                );
                continue;
            }
            let lowered = lowered.with_context(|| format!("{} {extension}", case.name))?;
            let again = lower(&source, lang, &env, mode)?;
            ensure!(
                lowered == again,
                "reproducible import: {} {extension}",
                case.name
            );
            // A misplaced caption is trivia, never a refusal: pin that every
            // caption a corpus source writes reaches the sidecar.
            ensure!(
                lowered.captions.entries().len() == written_captions(&source, lang),
                "{} {extension}: {} of {} written captions imported",
                case.name,
                lowered.captions.entries().len(),
                written_captions(&source, lang)
            );
            let document = &*lowered.document;
            let entries: Vec<_> = document
                .exports
                .iter()
                .filter_map(|(name, export)| {
                    matches!(export, k::Export::Function(_)).then_some(name)
                })
                .map(|name| name.as_str().to_owned())
                .collect();
            ensure!(
                entries == case.entries,
                "MODULE-CONTRACT: {} {extension}: entries {entries:?}",
                case.name
            );
            let map = readable::source_map(
                &lowered,
                env.library,
                &source,
                case.exceptions
                    .get(extension)
                    .context("dialect exceptions must be declared")?,
            )?;
            outputs.insert(format!("{prefix}source-map.{extension}.txt"), map);
            let shipped: Document = serde_json::from_slice(&serde_json::to_vec(document)?)?;
            let (content, link) = namespace.link(shipped.clone())?;
            let mut kernel = k::print_document(document);
            let mut views = String::new();
            let selected: Vec<_> = if entries.is_empty() {
                vec![StartSelector::Main]
            } else {
                entries
                    .iter()
                    .map(|name| StartSelector::Export(Name::new(name)))
                    .collect()
            };
            for entry in selected {
                let checked = WorkflowDocument::entered(
                    shipped.clone(),
                    LinkedStart {
                        link,
                        selector: entry,
                    },
                    env.library,
                )?;
                let mut context = lash_kernel_view::ViewContext::new(
                    checked.reference().clone(),
                    content.clone(),
                    Default::default(),
                    Arc::clone(&lash::vm::helper_release()?.helpers),
                )?
                .with_timer_operations(lash::vm::TIMER_OPERATIONS.iter().cloned());
                let release = lash::vm::helper_release()?;
                context.add_helper_labels(&release.labels)?;
                let view = lash_kernel_view::derive(&checked, &context)?;
                writeln!(
                    kernel,
                    "\nunit {:?}: statements={} scopes={} sites={}",
                    checked.entry_unit(),
                    view.statements.len(),
                    view.scopes.len(),
                    code_sites(document, env.library).len()
                )?;
                views.push_str(&readable::view(&checked, &view)?);
                views.push_str(&readable::captions(&checked, &view, &lowered.captions)?);
            }
            outputs.insert(format!("{prefix}kernel.{extension}.txt"), kernel);
            outputs.insert(format!("{prefix}view.{extension}.txt"), views);
            let printing = Printing {
                environment: &env,
                mode,
            };
            let source = roundtrip(document, &lowered.captions, lang, &printing)
                .with_context(|| format!("{} {extension}: authored print/read", case.name))?;
            report.push(Lifted::of(&case.name, lang, false, &source));
            if case.mode == CaseMode::Module
                && let Some(transaction) = canvas_edit(document, env.library)
            {
                let mut draft = Draft::open(document.clone())?;
                let applied = draft.apply(
                    &transaction,
                    &checker,
                    &[
                        lash_dialect_typescript::CONVENTIONS,
                        lash_dialect_python::CONVENTIONS,
                    ],
                )?;
                let rekeyed = lowered
                    .captions
                    .rekey(&applied.correspondence, document, draft.document())
                    .context("captions rekey through the canvas edit")?;
                let edited = roundtrip(draft.document(), &rekeyed.captions, lang, &printing)
                    .with_context(|| {
                        format!("{} {extension}: edited authored print/read", case.name)
                    })?;
                report.push(Lifted::of(&case.name, lang, true, &edited));
            }
            outputs.insert(format!("{prefix}printed.{extension}"), source);
            if case.edit {
                let (json, text) = edit(document, &lowered.captions, lang, &checker, &printing)?;
                outputs.insert(format!("{prefix}edits/1.{extension}.json"), json);
                outputs.insert(format!("{prefix}edits/1.{extension}.txt"), text);
            }
            if case.mode == CaseMode::Cell {
                let text = cell::crossing(&lowered, &embedding, &namespace)
                    .with_context(|| format!("{} {extension}: cell crossing", case.name))?;
                outputs.insert(format!("{prefix}cell-1.{extension}.txt"), text);
            }
        }
    }
    Ok((outputs, report))
}

/// The canvas edit the corpus records: the first tool argument of the first
/// step that performs a tool, set to a text; or, in a workflow with no such
/// argument, the first text a statement holds.
pub(crate) fn canvas_edit(
    document: &Document,
    library: &dyn k::FunctionCatalog,
) -> Option<Transaction> {
    use lash_kernel_edit::authoring::{DataValue, Operation, ValueTarget};
    let sites = code_sites(document, library);
    let argument = sites.iter().find(|site| match document.node(site) {
        Some(Node::Expr(Expr::Act(action))) => {
            matches!(&action.kind, k::ActKind::Perform { .. })
                && action
                    .args
                    .first()
                    .is_some_and(|value| plain(Node::Expr(value)))
        }
        _ => false,
    });
    let target = match argument {
        Some(action) => ValueTarget::ToolArgument {
            action: action.clone(),
            index: 0,
            path: vec![],
        },
        None => ValueTarget::Literal {
            expression: sites
                .iter()
                .find(|site| {
                    matches!(
                        document.node(site),
                        Some(Node::Stmt(
                            k::Stmt::Let {
                                value: k::Rhs::Expr(Expr::Literal(k::Literal::Text(_))),
                                ..
                            } | k::Stmt::Return {
                                value: Expr::Literal(k::Literal::Text(_))
                            } | k::Stmt::ReturnVia {
                                value: Expr::Literal(k::Literal::Text(_)),
                                ..
                            }
                        ))
                    )
                })?
                .child(0),
        },
    };
    let base = document.identity();
    Some(Transaction {
        base,
        edits: vec![Operation::SetValue {
            base,
            target,
            value: DataValue::Literal(k::Literal::Text("edited λ".into())),
        }],
    })
}

/// Whether the canvas may replace `node`: a value with no action or
/// closure in it.
fn plain(node: Node<'_>) -> bool {
    !matches!(
        node,
        Node::Expr(Expr::Act(_) | Expr::Closure(_)) | Node::Action(_)
    ) && node.children().into_iter().all(plain)
}

fn edit(
    document: &Document,
    captions: &Captions,
    lang: SourceLanguage,
    checker: &Checker<'_>,
    printing: &Printing<'_>,
) -> Result<(String, String)> {
    let transaction =
        canvas_edit(document, checker.functions).context("edit fixture needs a perform")?;
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Replay {
        transaction: Transaction,
    }
    let json = serde_json::to_string_pretty(&Replay { transaction })? + "\n";
    let replay: Replay = serde_json::from_str(&json)?;
    let mut draft = Draft::open(document.clone())?;
    let applied = draft.apply(
        &replay.transaction,
        checker,
        &[
            lash_dialect_typescript::CONVENTIONS,
            lash_dialect_python::CONVENTIONS,
        ],
    )?;
    let mut repeated = Draft::open(document.clone())?;
    let context = Checker::new(checker.functions);
    let again = repeated.apply(
        &replay.transaction,
        &context,
        &[
            lash_dialect_typescript::CONVENTIONS,
            lash_dialect_python::CONVENTIONS,
        ],
    )?;
    ensure!(
        draft.document().canonical_bytes() == repeated.document().canonical_bytes()
            && applied.correspondence == again.correspondence,
        "EDIT-CLOSED: expanded Transaction must replay exactly"
    );
    let before = k::print_document(document);
    let after = k::print_document(draft.document());
    let mut text = String::new();
    text.push_str("--- before\n");
    for line in before.lines() {
        writeln!(text, "-{line}")?;
    }
    text.push_str("+++ after\n");
    for line in after.lines() {
        writeln!(text, "+{line}")?;
    }
    writeln!(
        text,
        "\nCorrespondence:\n{}",
        serde_json::to_string_pretty(&applied.correspondence)?
    )?;
    // The host's sidecar follows the edit: survivors move with their steps,
    // and the ones whose step the edit wrote are marked for review.
    let rekeyed = captions
        .rekey(&applied.correspondence, document, draft.document())
        .context("captions rekey through the edit's correspondence")?;
    writeln!(text, "\nCaptions to review:")?;
    for site in &rekeyed.touched {
        let caption = rekeyed
            .captions
            .at(site)
            .context("a touched site keeps its caption")?;
        writeln!(text, "{site} {:?}", caption.title())?;
    }
    write!(
        text,
        "\nReprinted source:\n{}",
        roundtrip(draft.document(), &rekeyed.captions, lang, printing)?
    )?;
    Ok((json, text))
}

/// Shared picker manifest, embedded for hosts without a corpus directory.
pub const CATALOG_JSON: &str = include_str!("../catalog.json");

/// Embedded sources and optional cell witnesses for one manifest case.
/// A missing dialect has no source; it must not be replaced by a fabricated twin.
pub struct SourceRow {
    pub name: &'static str,
    pub typescript: Option<&'static str>,
    pub python: Option<&'static str>,
    pub typescript_witness: Option<&'static str>,
    pub python_witness: Option<&'static str>,
}

/// The manifest owns membership and entry/refusal contracts. Literal includes
/// make corpus assets available to a deployed host without filesystem reads.
pub const SOURCES: &[SourceRow] = &[
    SourceRow {
        name: "straight-tool",
        typescript: Some(include_str!("../straight-tool/source.ts")),
        python: Some(include_str!("../straight-tool/source.py")),
        typescript_witness: None,
        python_witness: None,
    },
    SourceRow {
        name: "branch",
        typescript: Some(include_str!("../branch/source.ts")),
        python: Some(include_str!("../branch/source.py")),
        typescript_witness: None,
        python_witness: None,
    },
    SourceRow {
        name: "iteration-typed",
        typescript: Some(include_str!("../iteration-typed/source.ts")),
        python: Some(include_str!("../iteration-typed/source.py")),
        typescript_witness: None,
        python_witness: None,
    },
    SourceRow {
        name: "iteration-untyped",
        typescript: Some(include_str!("../iteration-untyped/source.ts")),
        python: Some(include_str!("../iteration-untyped/source.py")),
        typescript_witness: None,
        python_witness: None,
    },
    SourceRow {
        name: "try-finally",
        typescript: Some(include_str!("../try-finally/source.ts")),
        python: Some(include_str!("../try-finally/source.py")),
        typescript_witness: None,
        python_witness: None,
    },
    SourceRow {
        name: "fan-out",
        typescript: Some(include_str!("../fan-out/source.ts")),
        python: Some(include_str!("../fan-out/source.py")),
        typescript_witness: None,
        python_witness: None,
    },
    SourceRow {
        name: "sleep",
        typescript: Some(include_str!("../sleep/source.ts")),
        python: Some(include_str!("../sleep/source.py")),
        typescript_witness: None,
        python_witness: None,
    },
    SourceRow {
        name: "closures-helpers",
        typescript: Some(include_str!("../closures-helpers/source.ts")),
        python: Some(include_str!("../closures-helpers/source.py")),
        typescript_witness: None,
        python_witness: None,
    },
    SourceRow {
        name: "early-return",
        typescript: Some(include_str!("../early-return/source.ts")),
        python: Some(include_str!("../early-return/source.py")),
        typescript_witness: None,
        python_witness: None,
    },
    SourceRow {
        name: "non-ascii",
        typescript: Some(include_str!("../non-ascii/source.ts")),
        python: Some(include_str!("../non-ascii/source.py")),
        typescript_witness: None,
        python_witness: None,
    },
    SourceRow {
        name: "entry-boundary",
        typescript: Some(include_str!("../entry-boundary/source.ts")),
        python: Some(include_str!("../entry-boundary/source.py")),
        typescript_witness: None,
        python_witness: None,
    },
    SourceRow {
        name: "early-finish",
        typescript: Some(include_str!("../early-finish/source.ts")),
        python: Some(include_str!("../early-finish/source.py")),
        typescript_witness: Some(include_str!("../early-finish/cell-1.ts.txt")),
        python_witness: Some(include_str!("../early-finish/cell-1.py.txt")),
    },
    SourceRow {
        name: "capturing-cell",
        typescript: Some(include_str!("../capturing-cell/source.ts")),
        python: Some(include_str!("../capturing-cell/source.py")),
        typescript_witness: Some(include_str!("../capturing-cell/cell-1.ts.txt")),
        python_witness: Some(include_str!("../capturing-cell/cell-1.py.txt")),
    },
    SourceRow {
        name: "refusal-duplicate",
        typescript: Some(include_str!("../refusal-duplicate/source.ts")),
        python: Some(include_str!("../refusal-duplicate/source.py")),
        typescript_witness: None,
        python_witness: None,
    },
    SourceRow {
        name: "refusal-empty",
        typescript: Some(include_str!("../refusal-empty/source.ts")),
        python: Some(include_str!("../refusal-empty/source.py")),
        typescript_witness: None,
        python_witness: None,
    },
    SourceRow {
        name: "probe-digest",
        typescript: Some(include_str!("../probe-digest/source.ts")),
        python: None,
        typescript_witness: None,
        python_witness: None,
    },
    SourceRow {
        name: "probe-typed",
        typescript: Some(include_str!("../probe-typed/source.ts")),
        python: None,
        typescript_witness: None,
        python_witness: None,
    },
    SourceRow {
        name: "probe-inbox",
        typescript: Some(include_str!("../probe-inbox/source.ts")),
        python: None,
        typescript_witness: None,
        python_witness: None,
    },
    SourceRow {
        name: "probe-flat",
        typescript: Some(include_str!("../probe-flat/source.ts")),
        python: None,
        typescript_witness: None,
        python_witness: None,
    },
    SourceRow {
        name: "probe-nested",
        typescript: Some(include_str!("../probe-nested/source.ts")),
        python: None,
        typescript_witness: None,
        python_witness: None,
    },
    SourceRow {
        name: "probe-retry",
        typescript: Some(include_str!("../probe-retry/source.ts")),
        python: None,
        typescript_witness: None,
        python_witness: None,
    },
    SourceRow {
        name: "probe-helper",
        typescript: Some(include_str!("../probe-helper/source.ts")),
        python: None,
        typescript_witness: None,
        python_witness: None,
    },
];
