//! Admitted canvas edits retain an authored inverse (FIG-7514).
use super::*;
use lash_kernel_edit::Position;
use lash_kernel_edit::authoring::{DataRecordEntry, DataValue, Operation, ValueTarget};

fn value(ty: &lash_kernel_value::Type) -> Option<DataValue> {
    use lash_kernel_value::Type;
    Some(match ty {
        Type::Any | Type::Json | Type::Text => {
            DataValue::Literal(k::Literal::Text("canvas".into()))
        }
        Type::Bool => DataValue::Literal(k::Literal::Bool(true)),
        Type::Int => DataValue::Literal(k::Literal::Int(1.into())),
        Type::Number | Type::Float => DataValue::Literal(k::Literal::Float(k::Float::new(1.0))),
        Type::Null => DataValue::Literal(k::Literal::Null),
        Type::Absent => DataValue::Literal(k::Literal::Absent),
        Type::List(_) => DataValue::List(vec![]),
        Type::Enum(items) => DataValue::Literal(k::Literal::Text(items.first()?.clone())),
        Type::Tuple(items) => DataValue::Tuple(items.iter().map(value).collect::<Option<_>>()?),
        Type::Set(_) => DataValue::Set(vec![]),
        Type::Map(_) => DataValue::Map(vec![]),
        Type::Record(record) => DataValue::Record(
            record
                .fields
                .iter()
                .filter(|f| !f.optional)
                .map(|f| {
                    Some(DataRecordEntry {
                        field: f.name.clone(),
                        value: value(&f.ty)?,
                    })
                })
                .collect::<Option<_>>()?,
        ),
        Type::Union(items) => return items.iter().find_map(value),
        _ => return None,
    })
}

pub(super) fn check(
    document: &Document,
    captions: &Captions,
    lang: SourceLanguage,
    checker: &Checker<'_>,
    printing: &Printing<'_>,
    partition: (usize, usize),
    steps: &BTreeMap<Site, lash_kernel_view::ActionView>,
) -> Result<()> {
    let base = document.identity();
    let mut positions = Vec::new();
    for (unit, block) in document.units() {
        let mut pending = vec![(Node::Block(block), Site::new(unit, vec![]))];
        while let Some((node, site)) = pending.pop() {
            if let Node::Block(block) = node {
                positions.extend((0..block.len()).map(|i| Position {
                    block: site.clone(),
                    before: Some(site.child(i as u32)),
                }));
                positions.push(Position::end(site.clone()));
            }
            pending.extend(
                (0u32..)
                    .zip(node.children())
                    .map(|(i, node)| (node, site.child(i))),
            );
        }
    }
    let mut operations = Vec::new();
    for (slot, import) in &document.imports {
        let lash_kernel_value::ImportKind::Operation { signature } = &import.kind else {
            continue;
        };
        let Some(args) = signature
            .params
            .iter()
            .map(|p| value(&p.ty))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        for at in &positions {
            for binding in [None, Some(Name::new("canvasInserted"))] {
                operations.push(Operation::InsertStep {
                    base,
                    at: at.clone(),
                    slot: *slot,
                    args: args.clone(),
                    result: lash_kernel_value::Type::Any,
                    binding,
                });
            }
        }
    }

    for action in document.action_ordinals().into_keys() {
        operations.push(Operation::RemoveStep {
            base,
            action: action.clone(),
        });
        let slot = match document.node(&action) {
            Some(Node::Expr(Expr::Act(act))) => match act.kind {
                k::ActKind::Perform { slot, .. } => slot,
                _ => continue,
            },
            Some(Node::Action(k::Action::Perform { slot, .. })) => *slot,
            _ => continue,
        };
        if let Some(lash_kernel_value::Import {
            kind: lash_kernel_value::ImportKind::Operation { signature },
            ..
        }) = document.imports.get(&slot)
            && let Some(param) = signature.params.first()
            && let Some(value) = value(&param.ty)
        {
            operations.push(Operation::SetValue {
                base,
                target: ValueTarget::ToolArgument {
                    action,
                    index: 0,
                    path: vec![],
                },
                value,
            });
        }
    }
    let source = lash_kernel_dialect::conventions::Conventions::of(
        document,
        printing.environment.library,
        lash::workflow::edit::CONVENTIONS,
    );
    let insertion_attempts = operations.iter().filter(|operation| {
        matches!(operation, Operation::InsertStep { slot, .. }
            if document.imports.get(slot).is_some_and(|import| source.operation_refusal(&import.name).is_none()))
    }).count();
    let mut failures = Vec::new();
    let mut admitted = 0;
    let mut inserted = 0;
    let mut edited = 0;
    let mut deleted = 0;
    let total = operations.len();
    let mut selected = 0;
    for (_, operation) in operations
        .into_iter()
        .enumerate()
        .filter(|(index, _)| index % partition.1 == partition.0)
    {
        selected += 1;
        let mut draft = Draft::open(document.clone())?;
        let applied = draft.apply(
            &Transaction {
                base,
                edits: vec![operation.clone()],
            },
            checker,
            lash::workflow::edit::CONVENTIONS,
        );
        let applied = match applied {
            Ok(applied) => applied,
            Err(refusal) => {
                if let Operation::RemoveStep { action, .. } = &operation
                    && let Some(step) = steps.get(action)
                {
                    let siblings = steps
                        .values()
                        .filter(|other| other.statement == step.statement)
                        .count();
                    let structural = matches!(
                        document.node(&step.statement),
                        Some(Node::Stmt(
                            k::Stmt::Do { .. } | k::Stmt::Let { .. } | k::Stmt::Assign { .. }
                        ))
                    );
                    let member_write = matches!(
                        document.node(&step.statement),
                        Some(Node::Stmt(k::Stmt::Assign {
                            place: k::Place::Member(_),
                            ..
                        }))
                    );
                    let mut pending = vec![(
                        document
                            .node(&step.statement)
                            .context("visible step statement")?,
                        step.statement.clone(),
                    )];
                    let mut sibling_computation = false;
                    while let Some((node, site)) = pending.pop() {
                        if !site.path.starts_with(&action.path)
                            && !action.path.starts_with(&site.path)
                            && matches!(node, Node::Action(_) | Node::Expr(Expr::Act(_)))
                        {
                            sibling_computation = true;
                        }
                        pending.extend(
                            node.children()
                                .into_iter()
                                .enumerate()
                                .map(|(i, node)| (node, site.child(i as u32))),
                        );
                    }
                    let semantic_refusal =
                        refusal
                            .diagnostics
                            .iter()
                            .all(|diagnostic| match diagnostic.kind {
                                lash_kernel_edit::EditDiagnosticKind::EscapingBinding
                                | lash_kernel_edit::EditDiagnosticKind::Refused(
                                    lash_kernel_check::RefusalReason::UnboundVariable { .. },
                                ) => true,
                                lash_kernel_edit::EditDiagnosticKind::NonSeparableStep {
                                    ..
                                } => {
                                    !structural
                                        || siblings > 1
                                        || member_write
                                        || sibling_computation
                                }
                                _ => false,
                            });
                    if !semantic_refusal {
                        failures.push(format!(
                            "visible {:?} deletion at {action}: {refusal}",
                            step.role
                        ));
                    }
                }
                continue;
            }
        };
        admitted += 1;
        match &operation {
            Operation::InsertStep { .. } => inserted += 1,
            Operation::SetValue { .. } => edited += 1,
            Operation::RemoveStep { .. } => deleted += 1,
            _ => unreachable!("the matrix contains inserts, edits and deletes"),
        }
        let rekeyed = captions
            .rekey(&applied.correspondence, document, draft.document())
            .context("canvas captions")?;
        // The corpus already checks print fixpoints. Each matrix row needs
        // one independent ordinary lowering of its admitted document's print.
        let checked = (|| -> Result<()> {
            let output = printed(draft.document(), &rekeyed.captions, lang, printing)?;
            let lowered = lower(&output.source, lang, printing.environment, printing.mode)?;
            ensure!(
                lowered.document.canonical_bytes() == draft.document().canonical_bytes(),
                "the printed canvas draft lowers to another document"
            );
            ensure!(
                lowered.captions.entries() == rekeyed.captions.entries(),
                "the printed canvas draft lowers with other captions"
            );
            Ok(())
        })();
        if let Err(error) = checked {
            failures.push(format!("{operation:?}: {error:#}"));
        }
    }
    ensure!(
        partition.1 > 1 || insertion_attempts == 0 || inserted > 0,
        "the canvas admitted no insertion in {lang:?}"
    );
    eprintln!(
        "canvas {lang:?} partition {partition:?}: selected={selected}/{total}, {admitted} admitted edits checked, insert={inserted} edit={edited} delete={deleted}"
    );
    ensure!(
        failures.is_empty(),
        "{} canvas deletion or print failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
    Ok(())
}
