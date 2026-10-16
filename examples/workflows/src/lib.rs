//! FIG-6384 / Design 8: readable, per-dialect workflow and cell witnesses.
//! The catalog is also the host example's source inventory. Generating an
//! output checks its semantic laws before any text can be blessed.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use lash_kernel_check::{Environment as Checker, WorkflowDocument};
use lash_kernel_dialect::{CellEnvironment, Environment, Lowered, Mode, PrintOptions};
use lash_kernel_doc::{
    self as k, Document, Expr, Name, Node, Site, SourceLanguage, Unit, WorkflowDocumentEntry,
};
use lash_kernel_edit::{Draft, Transaction};
use serde::{Deserialize, Serialize};

mod cell;
mod readable;

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

fn printed(
    document: &Document,
    lang: SourceLanguage,
) -> Result<lash_kernel_dialect::PrintedSource> {
    let options = PrintOptions {
        source_map: true,
        ..Default::default()
    };
    Ok(match lang {
        SourceLanguage::TypeScript => lash_dialect_typescript::print(document, options)?,
        SourceLanguage::Python => lash_dialect_python::print(document, options)?,
    })
}

fn roundtrip(document: &Document, lang: SourceLanguage) -> Result<String> {
    let output = printed(document, lang)?;
    let decoded = match lang {
        SourceLanguage::TypeScript => {
            lash_dialect_typescript::read(&output.source, document, Default::default())?
        }
        SourceLanguage::Python => {
            lash_dialect_python::read(&output.source, document, Default::default())?
        }
    };
    ensure!(
        decoded.canonical_bytes() == document.canonical_bytes(),
        "authored import -> print -> read changed canonical bytes"
    );
    output
        .source_map
        .context("SM-ORDER: printer returns a requested map")?
        .validate(document, &output.source)?;
    // SM-ORDER's editor handoff accepts precisely the printed document.
    Draft::open(document.clone())?;
    Ok(output.source)
}

pub(crate) fn code_sites(document: &Document) -> Vec<Site> {
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
    walk(
        Node::Block(&document.main),
        Site::new(Unit::Main, vec![]),
        &mut sites,
    );
    for (name, function) in &document.functions {
        walk(
            Node::Block(&function.body),
            Site::new(Unit::Function(name.clone()), vec![]),
            &mut sites,
        );
    }
    sites
}

fn effects() -> Result<BTreeMap<k::EffectName, k::Signature>> {
    ["echo", "finish"]
        .into_iter()
        .map(|name| {
            Ok((
                k::EffectName::new(name)?,
                k::Signature {
                    params: vec![k::Param {
                        name: Name::new("input"),
                        ty: k::Type::Any,
                        optional: false,
                    }],
                    result: k::Type::Any,
                },
            ))
        })
        .collect()
}

/// Generate review artifacts, checking reproducible import, exact source
/// reading, SM-ORDER/SM-COVER, closed edit replay and cell crossing structure.
/// Only entry names and refusal codes are compared between dialects.
#[expect(
    clippy::disallowed_methods,
    reason = "corpus tooling reads explicitly supplied fixture paths"
)]
pub fn render(root: &Path) -> Result<BTreeMap<String, String>> {
    let embedding = lash_vm_worker::built_dialects(&Default::default())?;
    let registry = embedding.registry()?;
    let effects = effects()?;
    let controls = BTreeMap::from([(
        k::EffectName::new("finish")?,
        BTreeSet::from([lash_kernel_dialect::EffectControl::Finish]),
    )]);
    let env = Environment {
        library: embedding.library(),
        effects: &effects,
        tool_roots: &BTreeSet::new(),
        controls: &controls,
    };
    let mut checker = Checker::new(registry.as_ref());
    checker.effects = effects.clone();
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
                    anyhow::bail!("MODULE-CONTRACT: refused case must refuse");
                };
                ensure!(
                    &diagnostic.code == expected,
                    "{} {extension}: expected {expected}, got {diagnostic}",
                    case.name
                );
                outputs.insert(
                    format!("{prefix}refusal.{extension}.txt"),
                    format!("code: {}\n", diagnostic.code),
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
            let document = &*lowered.document;
            let entries: Vec<_> = document
                .entries
                .keys()
                .map(|name| name.as_str().to_owned())
                .collect();
            ensure!(
                entries == case.entries,
                "MODULE-CONTRACT: {} {extension}: entries {entries:?}",
                case.name
            );
            let map = readable::source_map(
                &lowered,
                &source,
                case.exceptions
                    .get(extension)
                    .context("dialect exceptions must be declared")?,
            )?;
            outputs.insert(format!("{prefix}source-map.{extension}.txt"), map);
            let declarations = registry.export_declarations();
            // No parser, live registry or engine is available below this seam:
            // the same serialized document/declarations a stored read supplies.
            let shipped: Document = serde_json::from_slice(&serde_json::to_vec(document)?)?;
            let held: k::FunctionDeclarations =
                serde_json::from_slice(&serde_json::to_vec(&declarations)?)?;
            let mut kernel = k::print_document(document);
            let mut views = String::new();
            let selected: Vec<_> = if entries.is_empty() {
                vec![WorkflowDocumentEntry::Main]
            } else {
                entries
                    .iter()
                    .map(|name| WorkflowDocumentEntry::Entry {
                        function: Name::new(name),
                    })
                    .collect()
            };
            for entry in selected {
                let checked = WorkflowDocument::derive(shipped.clone(), entry, &held)?;
                let view = lash_kernel_view::derive(&checked)?;
                writeln!(
                    kernel,
                    "\nunit {:?}: statements={} scopes={} sites={}",
                    checked.entry_unit(),
                    view.statements.len(),
                    view.scopes.len(),
                    code_sites(document).len()
                )?;
                views.push_str(&readable::view(&checked, &view)?);
            }
            outputs.insert(format!("{prefix}kernel.{extension}.txt"), kernel);
            outputs.insert(format!("{prefix}view.{extension}.txt"), views);
            outputs.insert(
                format!("{prefix}printed.{extension}"),
                roundtrip(document, lang)?,
            );
            if case.edit {
                let (json, text) = edit(document, lang, &checker)?;
                outputs.insert(format!("{prefix}edits/1.{extension}.json"), json);
                outputs.insert(format!("{prefix}edits/1.{extension}.txt"), text);
            }
            if case.mode == CaseMode::Cell {
                let text = cell::crossing(&lowered, registry.clone())?;
                outputs.insert(format!("{prefix}cell-1.{extension}.txt"), text);
            }
        }
    }
    Ok(outputs)
}

fn edit(
    document: &Document,
    lang: SourceLanguage,
    checker: &Checker<'_>,
) -> Result<(String, String)> {
    use lash_kernel_edit::authoring::{DataValue, Operation, ValueTarget};
    let action = code_sites(document)
        .into_iter()
        .find(|site| {
            matches!(
                document.node(site),
                Some(Node::Action(k::Action::Perform { .. }))
            )
        })
        .context("edit fixture needs a perform")?;
    let base = document.identity();
    let transaction = Transaction {
        base,
        edits: vec![Operation::SetValue {
            base,
            target: ValueTarget::ToolArgument {
                action,
                index: 0,
                path: vec![],
            },
            value: DataValue::Literal(k::Literal::Text("edited λ".into())),
        }],
    };
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Replay {
        context: BTreeMap<k::EffectName, k::Signature>,
        transaction: Transaction,
    }
    let json = serde_json::to_string_pretty(&Replay {
        context: checker.effects.clone(),
        transaction,
    })? + "\n";
    let replay: Replay = serde_json::from_str(&json)?;
    let mut draft = Draft::open(document.clone())?;
    let applied = draft.apply(&replay.transaction, checker)?;
    let mut repeated = Draft::open(document.clone())?;
    let mut context = Checker::new(checker.functions);
    context.effects = replay.context;
    let again = repeated.apply(&replay.transaction, &context)?;
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
    write!(
        text,
        "\nReprinted source:\n{}",
        roundtrip(draft.document(), lang)?
    )?;
    Ok((json, text))
}
